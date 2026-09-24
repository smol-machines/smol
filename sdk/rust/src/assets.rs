//! Telling the engine where its runtime assets live.
//!
//! An embedded process has no install layout to fall back on, so a program that
//! ships its own boot binary, libraries or guest rootfs points the engine at
//! them once, before the first machine is created. Anything already set in the
//! environment wins, so an operator can override a build-time path without
//! recompiling.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::error::{Error, ErrorKind, Result};

/// Paths to the assets the engine needs.
///
/// Every field is optional. A field left unset keeps whatever the engine would
/// have resolved on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeAssets {
    /// The boot helper binary.
    pub boot_binary: Option<PathBuf>,
    /// Directory holding the hypervisor libraries.
    pub lib_dir: Option<PathBuf>,
    /// An extracted guest agent rootfs directory.
    pub agent_rootfs: Option<PathBuf>,
    /// A guest agent rootfs tarball, used only when `agent_rootfs` is unset.
    pub agent_rootfs_tar: Option<PathBuf>,
}

impl RuntimeAssets {
    /// Assets with nothing set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Use this boot helper binary.
    pub fn boot_binary(mut self, path: impl Into<PathBuf>) -> Self {
        self.boot_binary = Some(path.into());
        self
    }

    /// Load hypervisor libraries from this directory.
    pub fn lib_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.lib_dir = Some(path.into());
        self
    }

    /// Use this extracted guest agent rootfs.
    pub fn agent_rootfs(mut self, path: impl Into<PathBuf>) -> Self {
        self.agent_rootfs = Some(path.into());
        self
    }

    /// Use this guest agent rootfs tarball.
    pub fn agent_rootfs_tar(mut self, path: impl Into<PathBuf>) -> Self {
        self.agent_rootfs_tar = Some(path.into());
        self
    }

    /// Read the assets out of an installed smolvm directory.
    ///
    /// An install holds the real binary as `smolvm-bin` beside a `smolvm`
    /// launcher script, with the hypervisor libraries in `lib/`. The boot
    /// helper has to be the binary, not the script.
    pub fn from_install_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let binary = dir.join("smolvm-bin");
        let boot_binary = if binary.is_file() {
            binary
        } else {
            dir.join("smolvm")
        };
        let lib_dir = dir.join("lib");
        Self {
            boot_binary: Some(boot_binary),
            lib_dir: lib_dir.is_dir().then_some(lib_dir),
            ..Self::default()
        }
    }

    /// Find an installed smolvm on `PATH` whose version matches this SDK's
    /// engine, and read its assets.
    ///
    /// Returns `None` when no compatible install is on `PATH`, which is the
    /// caller's cue to ship its own assets instead. An older or newer install
    /// is passed over: its libraries and boot helper belong to a different
    /// engine release.
    pub fn from_path_lookup() -> Option<Self> {
        let path = std::env::var_os("PATH")?;
        let wanted = crate::bootstrap::engine_version();
        std::env::split_paths(&path)
            .map(|dir| dir.join("smolvm"))
            .filter(|candidate| candidate.is_file())
            .find(|candidate| {
                crate::transport::local::engine_version_of(candidate)
                    .is_some_and(|v| crate::bootstrap::is_compatible_engine(&v, &wanted))
            })
            .and_then(|candidate| std::fs::canonicalize(candidate).ok())
            .and_then(|resolved| resolved.parent().map(Self::from_install_dir))
    }
}

static CONFIGURED: OnceLock<Mutex<Option<RuntimeAssets>>> = OnceLock::new();

fn config_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Config, message)
}

fn resolve(key: &str, candidate: Option<PathBuf>, expect_dir: bool) -> Result<Option<PathBuf>> {
    let Some(path) = std::env::var_os(key).map(PathBuf::from).or(candidate) else {
        return Ok(None);
    };
    let resolved = path
        .canonicalize()
        .map_err(|error| config_error(format!("resolve {key} '{}': {error}", path.display())))?;
    let metadata = std::fs::metadata(&resolved).map_err(|error| {
        config_error(format!("inspect {key} '{}': {error}", resolved.display()))
    })?;
    if metadata.is_dir() != expect_dir {
        let expected = if expect_dir { "directory" } else { "file" };
        return Err(config_error(format!(
            "{key} '{}' must be a {expected}",
            resolved.display()
        )));
    }
    Ok(Some(resolved))
}

fn apply(key: &str, value: Option<&Path>) {
    if let Some(value) = value {
        std::env::set_var(key, value);
    }
}

/// Point the engine at its runtime assets.
///
/// Call this once, before creating a machine. Calling it again with the same
/// paths is a no-op; calling it with different paths is an error, because the
/// engine caches what it resolved on first use and a silent second answer would
/// be ignored rather than honoured.
///
/// An embedded process almost always has to set at least the boot binary. The
/// engine otherwise re-executes the current program to boot the VM, and your
/// program does not know how to be a VM, so the boot fails with nothing more
/// useful than a non-zero exit. [`RuntimeAssets::from_path_lookup`] covers the
/// common case of an installed smolvm.
pub fn configure_runtime_assets(assets: RuntimeAssets) -> Result<()> {
    let agent_rootfs = resolve("SMOLVM_AGENT_ROOTFS", assets.agent_rootfs, true)?;
    let resolved = RuntimeAssets {
        boot_binary: resolve("SMOLVM_BOOT_BINARY", assets.boot_binary, false)?,
        lib_dir: resolve("SMOLVM_LIB_DIR", assets.lib_dir, true)?,
        // A tarball is only consulted when no extracted rootfs was given.
        agent_rootfs_tar: if agent_rootfs.is_none() {
            resolve("SMOLVM_AGENT_ROOTFS_TAR", assets.agent_rootfs_tar, false)?
        } else {
            None
        },
        agent_rootfs,
    };

    let state = CONFIGURED.get_or_init(|| Mutex::new(None));
    let mut configured = state
        .lock()
        .map_err(|_| config_error("runtime asset configuration lock was poisoned"))?;
    if let Some(existing) = configured.as_ref() {
        if existing != &resolved {
            return Err(config_error(
                "runtime assets were already configured with different paths",
            ));
        }
        return Ok(());
    }

    // Deliberately NOT exported. `SMOLVM_BOOT_BINARY` tells the engine that an
    // in-process embedder owns the VM's lifetime, so the boot process arms a
    // parent-death watchdog and the VM dies with whatever started it. This SDK
    // drives the detached CLI, where every `machine start` is a short-lived
    // process — exporting it killed every machine seconds after it booted.
    // Use `SMOLVM` to choose which binary the SDK drives instead.
    let _ = &resolved.boot_binary;
    apply("SMOLVM_LIB_DIR", resolved.lib_dir.as_deref());
    apply("SMOLVM_AGENT_ROOTFS", resolved.agent_rootfs.as_deref());
    apply(
        "SMOLVM_AGENT_ROOTFS_TAR",
        resolved.agent_rootfs_tar.as_deref(),
    );
    *configured = Some(resolved);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_install_dir_prefers_the_binary_over_the_launcher_script() {
        let dir = std::env::temp_dir().join("smolmachines-assets-test");
        let lib = dir.join("lib");
        std::fs::create_dir_all(&lib).expect("create the fixture install");
        std::fs::write(dir.join("smolvm"), "#!/bin/sh\n").expect("write the launcher");
        std::fs::write(dir.join("smolvm-bin"), "").expect("write the binary");

        let assets = RuntimeAssets::from_install_dir(&dir);
        assert_eq!(assets.boot_binary, Some(dir.join("smolvm-bin")));
        assert_eq!(assets.lib_dir, Some(lib));

        std::fs::remove_dir_all(&dir).expect("clean up the fixture");
    }

    #[test]
    fn an_install_without_a_separate_binary_falls_back_to_the_launcher() {
        let dir = std::env::temp_dir().join("smolmachines-assets-test-plain");
        std::fs::create_dir_all(&dir).expect("create the fixture install");
        std::fs::write(dir.join("smolvm"), "").expect("write the launcher");

        let assets = RuntimeAssets::from_install_dir(&dir);
        assert_eq!(assets.boot_binary, Some(dir.join("smolvm")));
        assert_eq!(assets.lib_dir, None);

        std::fs::remove_dir_all(&dir).expect("clean up the fixture");
    }
}
