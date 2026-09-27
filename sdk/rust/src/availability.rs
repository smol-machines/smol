//! Whether this host can run local machines, answered without booting one.

use std::path::{Path, PathBuf};

use crate::error::{Error, ErrorKind, Result};

/// Check that this host can run local machines, without booting one.
///
/// A framework choosing a sandbox ("a local microVM if possible, otherwise
/// something else") would otherwise learn the answer only from a failed
/// [`crate::Machine::start`]. This runs the checks a boot would fail on and
/// returns the same [`ErrorKind`]:
///
/// - [`ErrorKind::NotSupported`]: no engine release for this OS, architecture
///   or libc (and `SMOLVM` does not name a binary to use instead);
/// - [`ErrorKind::Config`]: `SMOLVM` names a binary that does not exist;
/// - [`ErrorKind::KvmUnavailable`]: Linux without a usable `/dev/kvm`;
/// - [`ErrorKind::HypervisorUnavailable`]: macOS without Hypervisor.framework.
///
/// It downloads nothing: the engine is still fetched on first local use. The
/// cloud target has no host requirements.
pub fn local_availability() -> Result<()> {
    match std::env::var_os("SMOLVM").map(PathBuf::from) {
        Some(explicit) => {
            crate::transport::local::explicit_cli(explicit)?;
        }
        None => {
            crate::bootstrap::platform_slug()?;
            if cfg!(all(target_os = "linux", not(target_env = "gnu"))) {
                return Err(Error::new(
                    ErrorKind::NotSupported,
                    "the engine release needs glibc, and this program is built for another \
                     libc; set SMOLVM to a `smolvm` built for this host, or use the cloud target",
                ));
            }
        }
    }
    check_hypervisor()
}

#[cfg(target_os = "linux")]
fn check_hypervisor() -> Result<()> {
    check_kvm(Path::new("/dev/kvm"))
}

#[cfg(target_os = "macos")]
fn check_hypervisor() -> Result<()> {
    let output = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.hv_support"])
        .output();
    match output {
        Ok(output) if String::from_utf8_lossy(&output.stdout).trim() == "1" => Ok(()),
        _ => Err(Error::new(
            ErrorKind::HypervisorUnavailable,
            "Hypervisor.framework is not available on this Mac (kern.hv_support is not 1); \
             a macOS VM without nested virtualization cannot run local machines",
        )),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn check_hypervisor() -> Result<()> {
    // Only reachable with SMOLVM set on a platform with no engine release;
    // that binary's own start is the check.
    Ok(())
}

/// Open the KVM device the way the engine will: read-write.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn check_kvm(device: &Path) -> Result<()> {
    let unavailable = |reason: String| Err(Error::new(ErrorKind::KvmUnavailable, reason));
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => unavailable(format!(
            "{} does not exist: enable virtualization in the firmware and load the kvm module, \
             or use a host (or VM with nested virtualization) that has it",
            device.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => unavailable(format!(
            "no permission to open {} read-write: add this user to its group \
             (usually `sudo usermod -aG kvm $USER`, then log in again)",
            device.display()
        )),
        Err(e) => unavailable(format!("cannot open {}: {e}", device.display())),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_missing_device_is_reported_as_kvm_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let error = check_kvm(&dir.path().join("kvm")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::KvmUnavailable);
        assert!(error.message().contains("does not exist"), "{error}");
    }

    #[test]
    fn a_device_this_user_cannot_write_is_reported_with_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("kvm");
        std::fs::write(&device, "").unwrap();
        std::fs::set_permissions(&device, std::fs::Permissions::from_mode(0o400)).unwrap();
        // Root opens anything; the check is only meaningful as a normal user.
        if std::fs::OpenOptions::new()
            .write(true)
            .open(&device)
            .is_ok()
        {
            return;
        }
        let error = check_kvm(&device).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::KvmUnavailable);
        assert!(error.message().contains("kvm"), "{error}");
    }

    #[test]
    fn a_usable_device_passes() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("kvm");
        std::fs::write(&device, "").unwrap();
        check_kvm(&device).unwrap();
    }
}
