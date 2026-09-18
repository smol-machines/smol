//! NAPI-visible types mirroring smolvm Rust types.
//!
//! These structs are exposed to JavaScript via `#[napi(object)]` and include
//! conversion impls to/from the corresponding smolvm types.

use napi_derive::napi;
use smolvm::agent::{ExecEvent as AgentExecEvent, HostMount, VmResources};
use smolvm::data::network::PortMapping;
use smolvm::data::resources::{DEFAULT_MICROVM_CPU_COUNT, DEFAULT_MICROVM_MEMORY_MIB};

// ============================================================================
// Input types (JS → Rust)
// ============================================================================

/// Absolute live-resize targets. Floating-point inputs are validated before
/// conversion so JavaScript numbers cannot silently wrap into smaller targets.
#[napi(object)]
pub struct ResizeOptions {
    pub cpus: Option<f64>,
    pub memory_mb: Option<f64>,
    pub storage_gb: Option<f64>,
    pub overlay_gb: Option<f64>,
}

fn resize_integer(value: Option<f64>, name: &str, maximum: f64) -> napi::Result<Option<u64>> {
    value
        .map(|value| {
            if !value.is_finite() || value.fract() != 0.0 || value < 1.0 || value > maximum {
                return Err(napi::Error::from_reason(format!(
                    "{name} must be a positive integer no larger than {maximum}"
                )));
            }
            Ok(value as u64)
        })
        .transpose()
}

impl ResizeOptions {
    pub fn into_spec(self) -> napi::Result<smolvm::embedded::ResizeSpec> {
        Ok(smolvm::embedded::ResizeSpec {
            cpus: resize_integer(self.cpus, "cpus", u8::MAX as f64)?.map(|v| v as u8),
            memory_mib: resize_integer(self.memory_mb, "memoryMb", u32::MAX as f64)?
                .map(|v| v as u32),
            storage_gib: resize_integer(self.storage_gb, "storageGb", (u64::MAX >> 30) as f64)?,
            overlay_gib: resize_integer(self.overlay_gb, "overlayGb", (u64::MAX >> 30) as f64)?,
        })
    }
}

#[cfg(test)]
mod resize_tests {
    use super::*;

    #[test]
    fn resize_numbers_do_not_truncate_or_wrap() {
        for invalid in [f64::NAN, f64::INFINITY, -1.0, 0.0, 1.5, 256.0, 4294967297.0] {
            assert!(resize_integer(Some(invalid), "cpus", 255.0).is_err());
        }
        assert_eq!(resize_integer(None, "cpus", 255.0).unwrap(), None);
        assert_eq!(
            resize_integer(Some(255.0), "cpus", 255.0).unwrap(),
            Some(255)
        );
        let spec = ResizeOptions {
            cpus: Some(4.0),
            memory_mb: None,
            storage_gb: None,
            overlay_gb: None,
        }
        .into_spec()
        .unwrap();
        assert_eq!(spec.cpus, Some(4));
        assert_eq!(spec.memory_mib, None);
        assert!(resize_integer(Some(17179869184.0), "storageGb", (u64::MAX >> 30) as f64).is_err());
    }
}

/// Paths to the runtime assets bundled with the JavaScript package.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct RuntimeAssets {
    /// `_boot-vm`-capable helper executable.
    pub boot_binary: Option<String>,
    /// Directory containing libkrun and libkrunfw.
    pub lib_dir: Option<String>,
    /// Already-extracted guest agent rootfs.
    pub agent_rootfs: Option<String>,
    /// Guest agent rootfs tarball to extract on first use.
    pub agent_rootfs_tar: Option<String>,
}

/// Configuration for creating a machine.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct MachineConfig {
    /// Unique machine name. Used as the VM identifier.
    pub name: String,
    /// OCI image to boot, when creating an image-backed machine.
    pub image: Option<String>,
    /// Environment for the image workload launched at machine start.
    pub env: Option<Vec<EnvVar>>,
    /// Working directory for the image workload.
    pub workdir: Option<String>,
    /// Run the workload as this user (image user name or `uid[:gid]`).
    pub user: Option<String>,
    /// Host directories to mount into the VM.
    pub mounts: Option<Vec<HostMountConfig>>,
    /// Port mappings from host to guest.
    pub ports: Option<Vec<PortMappingConfig>>,
    /// VM resource allocation.
    pub resources: Option<VmResourcesConfig>,
    /// If true, the DB record is kept as a persistent machine.
    pub persistent: Option<bool>,
    /// If true, every start uses cloneable, memfd-backed guest RAM.
    pub forkable: Option<bool>,
}

/// A host directory mount specification.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct HostMountConfig {
    /// Absolute path on the host.
    pub source: String,
    /// Absolute path inside the guest.
    pub target: String,
    /// Mount as read-only (default: false — writable, matching the CLI).
    pub read_only: Option<bool>,
    /// Use a guest-local working copy and synchronize changes in batches.
    pub staged: Option<bool>,
}

/// A port mapping from host to guest.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct PortMappingConfig {
    /// Port on the host.
    pub host: u16,
    /// Port inside the guest.
    pub guest: u16,
}

/// VM resource allocation.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct VmResourcesConfig {
    /// Number of vCPUs (default: src/data/resources.rs default).
    pub cpus: Option<u8>,
    /// Memory in MiB (default: src/data/resources.rs default).
    pub memory_mib: Option<u32>,
    /// Enable outbound network access (default: false).
    pub network: Option<bool>,
    /// Storage disk size in GiB (default: 20).
    pub storage_gib: Option<f64>,
    /// Overlay disk size in GiB (default: 10).
    pub overlay_gib: Option<f64>,
    /// Enable GPU acceleration (virtio-gpu/venus). Local target only (default: false).
    pub gpu: Option<bool>,
    /// GPU VRAM in MiB (default: engine default when GPU is enabled).
    pub gpu_vram_mib: Option<u32>,
    /// Run the guest's unmodified CUDA/PyTorch code on the host's NVIDIA GPU by
    /// remoting CUDA calls over vsock (distinct from `gpu`, which is Vulkan; no
    /// CUDA toolkit needed in the image). Local target only (default: false).
    pub cuda: Option<bool>,
}

/// Options for executing a command.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// Environment variables as key-value pairs.
    pub env: Option<Vec<EnvVar>>,
    /// Working directory inside the VM/container.
    pub workdir: Option<String>,
    /// Timeout in seconds.
    pub timeout_secs: Option<u32>,
}

/// Options for writing a file into the VM.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct FileWriteOptions {
    /// Optional octal file mode (for example, 0o644).
    pub mode: Option<u32>,
}

/// An environment variable key-value pair.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

// ============================================================================
// Output types (Rust → JS)
// ============================================================================

/// Result of executing a command.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// Process exit code.
    pub exit_code: i32,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

/// Information about a pulled/cached OCI image.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ImageInfo {
    /// Image reference (e.g., "alpine:latest").
    pub reference: String,
    /// Image digest (sha256:...).
    pub digest: String,
    /// Image size in bytes.
    pub size: f64,
    /// Platform architecture (e.g., "arm64").
    pub architecture: String,
    /// Platform OS (e.g., "linux").
    pub os: String,
}

/// Result of writing a portable live checkpoint to local disk.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct LocalCheckpointResult {
    /// Compressed artifact size in bytes.
    pub size_bytes: f64,
    /// Source pause at the RAM/disk consistency boundary, in milliseconds.
    pub source_pause_ms: f64,
    /// Complete capture and compression time, in milliseconds.
    pub elapsed_ms: f64,
}

/// Event from a streaming exec session.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ExecStreamEvent {
    /// Event kind: stdout, stderr, exit, or error.
    pub kind: String,
    /// Text payload for stdout/stderr events.
    pub data: Option<String>,
    /// Exit code for exit events.
    pub exit_code: Option<i32>,
    /// Error message for error events.
    pub message: Option<String>,
}

// ============================================================================
// Conversion impls
// ============================================================================

impl TryFrom<&HostMountConfig> for HostMount {
    type Error = smolvm::error::Error;

    fn try_from(m: &HostMountConfig) -> Result<Self, Self::Error> {
        // Default writable, matching the engine's own `HostMount::parse`
        // (host:guest[:ro|:rw] defaults to writable) and the `smol -v` CLI. A
        // read-only mount is opt-in via read_only: true.
        let read_only = m.read_only.unwrap_or(false);
        let staged = m.staged.unwrap_or(false);
        if read_only && staged {
            return Err(smolvm::Error::invalid_mount_path(
                "a staged mount is writable and cannot also be read-only",
            ));
        }
        let mut mount = HostMount::new(&m.source, &m.target, read_only)?;
        mount.staged = staged;
        Ok(mount)
    }
}

impl From<&PortMappingConfig> for PortMapping {
    fn from(p: &PortMappingConfig) -> Self {
        PortMapping::new(p.host, p.guest)
    }
}

impl VmResourcesConfig {
    pub fn to_vm_resources(&self) -> VmResources {
        VmResources {
            cpus: self.cpus.unwrap_or(DEFAULT_MICROVM_CPU_COUNT),
            memory_mib: self.memory_mib.unwrap_or(DEFAULT_MICROVM_MEMORY_MIB),
            network: self.network.unwrap_or(false),
            storage_gib: self.storage_gib.map(|g| g as u64),
            overlay_gib: self.overlay_gib.map(|g| g as u64),
            gpu: self.gpu.unwrap_or(false),
            gpu_vram_mib: self.gpu_vram_mib,
            cuda: self.cuda.unwrap_or(false),
            ..Default::default()
        }
    }
}

impl From<smolvm_protocol::ImageInfo> for ImageInfo {
    fn from(info: smolvm_protocol::ImageInfo) -> Self {
        ImageInfo {
            reference: info.reference,
            digest: info.digest,
            size: info.size as f64,
            architecture: info.architecture,
            os: info.os,
        }
    }
}

impl From<AgentExecEvent> for ExecStreamEvent {
    fn from(event: AgentExecEvent) -> Self {
        match event {
            AgentExecEvent::Stdout(data) => Self {
                kind: "stdout".to_string(),
                data: Some(String::from_utf8_lossy(&data).into_owned()),
                exit_code: None,
                message: None,
            },
            AgentExecEvent::Stderr(data) => Self {
                kind: "stderr".to_string(),
                data: Some(String::from_utf8_lossy(&data).into_owned()),
                exit_code: None,
                message: None,
            },
            AgentExecEvent::Exit(exit_code) => Self {
                kind: "exit".to_string(),
                data: None,
                exit_code: Some(exit_code),
                message: None,
            },
            AgentExecEvent::Error(message) => Self {
                kind: "error".to_string(),
                data: None,
                exit_code: None,
                message: Some(message),
            },
        }
    }
}

/// Parse ExecOptions into the components needed by AgentClient::vm_exec().
pub fn parse_exec_options(
    options: Option<ExecOptions>,
) -> (
    Vec<(String, String)>,
    Option<String>,
    Option<std::time::Duration>,
) {
    match options {
        Some(opts) => {
            let env = opts
                .env
                .map(|vars| vars.into_iter().map(|v| (v.key, v.value)).collect())
                .unwrap_or_default();

            let timeout = opts
                .timeout_secs
                .map(|s| std::time::Duration::from_secs(s as u64));

            (env, opts.workdir, timeout)
        }
        None => (Vec::new(), None, None),
    }
}
