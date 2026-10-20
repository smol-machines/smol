//! Host mounts a Smolfile declares are untrusted input.
//!
//! A Smolfile usually arrives inside the repository whose code the machine is
//! meant to contain, so its `volumes` are chosen by whoever wrote the repo, not
//! by whoever runs `smol file up`. Before any of them is shared:
//!
//! - a mount with no explicit mode is read-only (`:rw` opts into writes);
//! - a source outside the Smolfile's directory is refused unless the operator
//!   names it with `--allow-mount`;
//! - the operator approves the exact set of mounts once per Smolfile, and the
//!   answer is recorded so an unchanged project starts without asking again.

use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// File under the user's config dir holding approved mount sets, keyed by the
/// Smolfile's canonical path.
const APPROVALS_FILE: &str = "smol/approved-smolfile-mounts.json";

/// One vetted mount: absolute source, guest target and an explicit mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VettedMount {
    pub source: PathBuf,
    pub target: String,
    pub mode: &'static str,
}

impl VettedMount {
    /// The `host:guest:mode` spec the engine's mount parser takes.
    pub fn spec(&self) -> String {
        format!("{}:{}:{}", self.source.display(), self.target, self.mode)
    }
}

/// Resolve each Smolfile mount spec against `project_dir`, default it to
/// read-only, and refuse a source outside the project unless it lies under one
/// of `allowed` (the operator's `--allow-mount` paths).
pub fn vet(
    specs: &[String],
    project_dir: &Path,
    allowed: &[PathBuf],
) -> anyhow::Result<Vec<VettedMount>> {
    let project = canonical(project_dir)?;
    let allowed = allowed
        .iter()
        .map(|p| canonical(p))
        .collect::<anyhow::Result<Vec<_>>>()?;
    specs
        .iter()
        .map(|spec| {
            // Same right-to-left split as the engine, so a Windows drive colon
            // stays in the source.
            let (rest, mode) = match spec.rsplit_once(':') {
                Some((head, "ro")) => (head, "ro"),
                Some((head, "rw")) => (head, "rw"),
                Some((head, "staged")) => (head, "staged"),
                _ => (spec.as_str(), "ro"),
            };
            let (source, target) = match rest.rsplit_once(':') {
                Some((s, t)) if !s.is_empty() && !t.is_empty() => (s, t),
                _ => anyhow::bail!(
                    "invalid Smolfile volume '{spec}' (expected host:guest[:ro|:rw|:staged])"
                ),
            };
            let source = canonical(&project.join(source))?;
            if !source.starts_with(&project) && !allowed.iter().any(|a| source.starts_with(a)) {
                anyhow::bail!(
                    "Smolfile volume '{spec}' shares {}, which is outside the project directory {}.\n\
                     A Smolfile can only share its own tree. To share this directory anyway, \
                     pass it on the command line: smol file up --allow-mount {}",
                    source.display(),
                    project.display(),
                    source.display()
                );
            }
            Ok(VettedMount {
                source,
                target: target.to_string(),
                mode,
            })
        })
        .collect()
}

fn canonical(path: &Path) -> anyhow::Result<PathBuf> {
    path.canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot resolve '{}': {e}", path.display()))
}

/// Ask the operator to approve `mounts` for the Smolfile at `smolfile`, unless
/// this exact set was approved before or `assume_yes` is set. Records the
/// approval. Fails without a terminal to ask on.
pub fn confirm(smolfile: &Path, mounts: &[VettedMount], assume_yes: bool) -> anyhow::Result<()> {
    if mounts.is_empty() {
        return Ok(());
    }
    let key = canonical(smolfile)?.display().to_string();
    let wanted: Vec<String> = mounts.iter().map(VettedMount::spec).collect();
    let store = approvals_path();
    let mut approvals = store.as_deref().map(load_approvals).unwrap_or_default();
    if approvals.get(&key) == Some(&wanted) {
        return Ok(());
    }

    if !assume_yes {
        let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        let mut err = std::io::stderr().lock();
        writeln!(err, "{key} shares these host directories with the machine:")?;
        for m in mounts {
            let note = match m.mode {
                "rw" => "  (writable: code in the machine can change these files)",
                "staged" => "  (copied in)",
                _ => "",
            };
            writeln!(
                err,
                "  {} -> {} [{}]{note}",
                m.source.display(),
                m.target,
                m.mode
            )?;
        }
        if !interactive {
            anyhow::bail!(
                "these mounts have not been approved for this Smolfile; run `smol file up` in a \
                 terminal to review them, or pass --yes to approve them"
            );
        }
        write!(err, "Allow? [y/N] ")?;
        err.flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
            anyhow::bail!("not starting: the Smolfile's mounts were not approved");
        }
    }

    approvals.insert(key, wanted);
    if let Some(store) = store {
        if let Err(e) = save_approvals(&store, &approvals) {
            eprintln!("warning: could not record the mount approval: {e}");
        }
    }
    Ok(())
}

fn approvals_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(APPROVALS_FILE))
}

fn load_approvals(path: &Path) -> BTreeMap<String, Vec<String>> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_approvals(path: &Path, approvals: &BTreeMap<String, Vec<String>>) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(approvals)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("repo");
        std::fs::create_dir_all(project.join("src")).unwrap();
        (dir, project)
    }

    #[test]
    fn a_mount_without_a_mode_is_read_only() {
        let (_dir, project) = project();
        let vetted = vet(&["./src:/app".to_string()], &project, &[]).unwrap();
        assert_eq!(vetted[0].mode, "ro");
        assert_eq!(
            vetted[0].source,
            project.join("src").canonicalize().unwrap()
        );
        let vetted = vet(&["src:/app:rw".to_string()], &project, &[]).unwrap();
        assert_eq!(vetted[0].mode, "rw", "writes need an explicit :rw");
    }

    #[test]
    fn a_source_outside_the_project_is_refused_unless_allowed() {
        let (dir, project) = project();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let spec = format!("{}:/host", home.display());
        let err = vet(std::slice::from_ref(&spec), &project, &[]).unwrap_err();
        assert!(
            err.to_string().contains("outside the project directory"),
            "{err}"
        );
        let escape = vet(&["../home:/host".to_string()], &project, &[]).unwrap_err();
        assert!(escape.to_string().contains("--allow-mount"), "{escape}");

        let vetted = vet(&[spec], &project, std::slice::from_ref(&home)).unwrap();
        assert_eq!(vetted[0].source, home.canonicalize().unwrap());
    }

    #[test]
    fn the_whole_project_may_be_shared() {
        let (_dir, project) = project();
        let vetted = vet(&[".:/work:rw".to_string()], &project, &[]).unwrap();
        assert_eq!(vetted[0].source, project.canonicalize().unwrap());
    }

    #[test]
    fn an_approval_is_recorded_per_exact_mount_set() {
        let (dir, project) = project();
        let store = dir.path().join("approvals.json");
        let mounts = vet(&["src:/app".to_string()], &project, &[]).unwrap();
        let mut approvals = BTreeMap::new();
        approvals.insert(
            "k".to_string(),
            mounts.iter().map(VettedMount::spec).collect::<Vec<_>>(),
        );
        save_approvals(&store, &approvals).unwrap();
        let loaded = load_approvals(&store);
        assert_eq!(loaded, approvals);
        let widened = vet(&["src:/app:rw".to_string()], &project, &[]).unwrap();
        assert_ne!(
            loaded.get("k"),
            Some(&widened.iter().map(VettedMount::spec).collect::<Vec<_>>()),
            "making a mount writable is a different set and asks again"
        );
    }
}
