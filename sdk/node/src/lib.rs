//! smolvm-napi — NAPI-RS bindings for the smolvm microVM runtime.
//!
//! This crate provides native Node.js bindings via NAPI-RS, allowing users
//! to create, manage, and interact with microVMs directly from Node.js
//! without requiring the `smolvm serve` daemon.
//!
//! # Architecture
//!
//! ```text
//! TypeScript API layer (ergonomic, API-compatible with smolvm-node)
//!   └── @smolvm/native .node binary (this crate)
//!         └── smolvm library (Rust)
//!               └── libkrun (dynamic linking) → Hypervisor.framework / KVM
//! ```

#[path = "errors.rs"]
pub mod error;
pub mod machine;
pub mod types;

use napi_derive::napi;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::types::RuntimeAssets;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedRuntimeAssets {
    boot_binary: Option<PathBuf>,
    lib_dir: Option<PathBuf>,
    agent_rootfs: Option<PathBuf>,
    agent_rootfs_tar: Option<PathBuf>,
}

static RUNTIME_ASSETS: OnceLock<Mutex<Option<ResolvedRuntimeAssets>>> = OnceLock::new();

fn napi_error(message: impl Into<String>) -> napi::Error {
    napi::Error::from_reason(message.into())
}

fn resolve_asset(
    environment_key: &str,
    candidate: Option<String>,
    expected_directory: bool,
) -> napi::Result<Option<PathBuf>> {
    let Some(path) = std::env::var_os(environment_key)
        .map(PathBuf::from)
        .or_else(|| candidate.map(PathBuf::from))
    else {
        return Ok(None);
    };
    let resolved = path.canonicalize().map_err(|error| {
        napi_error(format!(
            "resolve {environment_key} '{}': {error}",
            path.display()
        ))
    })?;
    let metadata = std::fs::metadata(&resolved).map_err(|error| {
        napi_error(format!(
            "inspect {environment_key} '{}': {error}",
            resolved.display()
        ))
    })?;
    if metadata.is_dir() != expected_directory {
        let expected = if expected_directory {
            "directory"
        } else {
            "file"
        };
        return Err(napi_error(format!(
            "{environment_key} '{}' must be a {expected}",
            resolved.display()
        )));
    }
    Ok(Some(resolved))
}

fn set_asset(environment_key: &str, value: Option<&Path>) {
    if let Some(value) = value {
        // Node synchronizes `process.env` with the native process environment,
        // but Bun currently keeps JavaScript assignments in its own environment
        // map. Set the same value from Rust before the embedded runtime starts so
        // libkrun discovery and the boot-helper subprocess behave identically.
        std::env::set_var(environment_key, value);
    }
}

/// Configure the embedded runtime from assets resolved by the JavaScript package.
///
/// This must run before the first `NapiMachine` is created. It deliberately
/// crosses the N-API boundary instead of relying only on JavaScript
/// `process.env`, which is not inherited by native subprocesses under Bun.
#[napi]
pub fn configure_runtime_assets(assets: RuntimeAssets) -> napi::Result<()> {
    let resolved_rootfs = resolve_asset("SMOLVM_AGENT_ROOTFS", assets.agent_rootfs, true)?;
    let resolved = ResolvedRuntimeAssets {
        boot_binary: resolve_asset("SMOLVM_BOOT_BINARY", assets.boot_binary, false)?,
        lib_dir: resolve_asset("SMOLVM_LIB_DIR", assets.lib_dir, true)?,
        agent_rootfs_tar: if resolved_rootfs.is_none() {
            resolve_asset("SMOLVM_AGENT_ROOTFS_TAR", assets.agent_rootfs_tar, false)?
        } else {
            None
        },
        agent_rootfs: resolved_rootfs,
    };

    let state = RUNTIME_ASSETS.get_or_init(|| Mutex::new(None));
    let mut configured = state
        .lock()
        .map_err(|_| napi_error("runtime asset configuration lock was poisoned"))?;
    if let Some(existing) = configured.as_ref() {
        if existing != &resolved {
            return Err(napi_error(
                "runtime assets were already configured with different paths",
            ));
        }
        return Ok(());
    }

    // The boot helper, library directory and rootfs tarball go to the engine
    // in-process. Exporting them into the environment made every `smolvm` CLI
    // this program spawned boot its machines with our helper, which exits as
    // soon as its parent does.
    smolvm::embedded::bundle::set_bundle(smolvm::embedded::bundle::Bundle {
        boot_binary: resolved.boot_binary.clone(),
        lib_dir: resolved.lib_dir.clone(),
        agent_rootfs_tar: resolved.agent_rootfs_tar.clone(),
    })
    .map_err(|rejected| {
        napi_error(format!(
            "the engine's bundle paths were already set differently: {rejected:?}"
        ))
    })?;
    // A rootfs *directory* has no in-process channel yet; it stays an environment variable.
    set_asset("SMOLVM_AGENT_ROOTFS", resolved.agent_rootfs.as_deref());
    *configured = Some(resolved);
    Ok(())
}
