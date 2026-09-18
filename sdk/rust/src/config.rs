//! Describing a machine before it exists.
//!
//! [`MachineConfig`] is a plain struct with a builder in front of it, so simple
//! cases stay one chained expression and complex ones can still be assembled
//! field by field and reused.

use smolvm::agent::{HostMount, VmResources};
use smolvm::data::network::PortMapping;
use smolvm::data::resources::{DEFAULT_MICROVM_CPU_COUNT, DEFAULT_MICROVM_MEMORY_MIB};
use smolvm::embedded::MachineSpec;

use crate::error::{Error, ErrorKind, Result};

/// A directory exposed inside the guest.
///
/// A local `source` is bind-mounted from the host. An `s3://` source is fetched
/// by the in-guest agent instead, and is routed to the engine's remote-volume
/// list automatically, so both kinds are declared the same way here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// Host directory, or an `s3://` URL.
    pub source: String,
    /// Absolute path inside the guest.
    pub target: String,
    /// Mount the guest view read-only.
    pub read_only: bool,
    /// Copy the source into guest-local storage at start, rather than sharing
    /// it live. Fast for build trees; call [`crate::Machine::sync`] to copy
    /// changes back. Local sources only.
    pub staged: bool,
}

impl Mount {
    /// A writable, live-shared mount.
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            read_only: false,
            staged: false,
        }
    }

    /// Make the guest view read-only.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Stage the source into guest-local storage instead of sharing it live.
    pub fn staged(mut self) -> Self {
        self.staged = true;
        self
    }

    fn is_remote(&self) -> bool {
        smolvm::remote_volume::is_remote_source(&self.source)
    }
}

/// An inbound port forward from the host into the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Port {
    /// Port listened on by the host.
    pub host: u16,
    /// Port the guest serves on.
    pub guest: u16,
}

impl Port {
    /// Forward `host` on the host to `guest` in the VM.
    pub fn new(host: u16, guest: u16) -> Self {
        Self { host, guest }
    }

    /// Forward a port straight through, host and guest alike.
    pub fn same(port: u16) -> Self {
        Self::new(port, port)
    }
}

/// What the VM is given. Every field falls back to the engine's default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resources {
    /// Virtual CPUs.
    pub cpus: Option<u8>,
    /// Guest RAM in MiB.
    pub memory_mib: Option<u32>,
    /// Give the guest outbound networking.
    pub network: Option<bool>,
    /// Size of the machine's persistent data disk, in GiB.
    pub storage_gib: Option<u64>,
    /// Size of the image overlay, in GiB.
    pub overlay_gib: Option<u64>,
    /// Expose a GPU to the guest.
    pub gpu: Option<bool>,
    /// GPU video memory in MiB.
    pub gpu_vram_mib: Option<u32>,
    /// Expose CUDA via API remoting.
    pub cuda: Option<bool>,
}

impl Resources {
    fn to_vm_resources(&self) -> VmResources {
        VmResources {
            cpus: self.cpus.unwrap_or(DEFAULT_MICROVM_CPU_COUNT),
            memory_mib: self.memory_mib.unwrap_or(DEFAULT_MICROVM_MEMORY_MIB),
            network: self.network.unwrap_or(false),
            storage_gib: self.storage_gib,
            overlay_gib: self.overlay_gib,
            gpu: self.gpu.unwrap_or(false),
            gpu_vram_mib: self.gpu_vram_mib,
            cuda: self.cuda.unwrap_or(false),
            ..Default::default()
        }
    }
}

/// Everything needed to create a machine.
#[derive(Debug, Clone, Default)]
pub struct MachineConfig {
    /// Unique machine name.
    pub name: String,
    /// OCI image to boot. Without one the guest comes up as a bare VM.
    pub image: Option<String>,
    /// Command overriding the image entrypoint.
    pub command: Vec<String>,
    /// Environment for the workload.
    pub env: Vec<(String, String)>,
    /// Working directory for the workload.
    pub workdir: Option<String>,
    /// User the workload runs as.
    pub user: Option<String>,
    /// Directories exposed in the guest.
    pub mounts: Vec<Mount>,
    /// Inbound port forwards.
    pub ports: Vec<Port>,
    /// VM sizing and devices.
    pub resources: Resources,
    /// Caller metadata. The engine never interprets these.
    pub labels: Vec<(String, String)>,
    /// Hostnames the egress filter permits.
    pub allowed_hosts: Vec<String>,
    /// Keep the machine's disks across stop and start.
    pub persistent: bool,
    /// Back guest RAM with a memfd on every start, so the machine can be
    /// branched.
    pub branchable: bool,
    /// CIDRs the machine may reach. Cloud only; locally, egress is governed by
    /// the network flag and the host.
    pub allowed_cidrs: Vec<String>,
    /// Idle seconds before the machine stops on its own. Cloud only.
    pub auto_stop_seconds: Option<u64>,
    /// Hard lifetime, after which the machine is deleted. Cloud only.
    pub ttl_seconds: Option<u64>,
}

impl MachineConfig {
    /// Start a config for a machine of this name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    /// Translate into the engine's spec, splitting host mounts from remote
    /// volumes. The workload environment travels beside the spec because the
    /// engine takes it as separate arguments.
    pub(crate) fn into_request(self) -> Result<CreateRequest> {
        let (remote, local): (Vec<Mount>, Vec<Mount>) =
            self.mounts.into_iter().partition(Mount::is_remote);

        let mut mounts = Vec::with_capacity(local.len());
        for mount in local {
            if mount.read_only && mount.staged {
                return Err(Error::new(
                    ErrorKind::Mount,
                    "a staged mount is writable and cannot also be read-only",
                ));
            }
            let mut host_mount = HostMount::new(&mount.source, &mount.target, mount.read_only)?;
            host_mount.staged = mount.staged;
            mounts.push(host_mount);
        }

        let mut remote_volumes = Vec::with_capacity(remote.len());
        for mount in remote {
            if mount.staged {
                return Err(Error::new(
                    ErrorKind::Mount,
                    "staged mode is only supported for local host directories",
                ));
            }
            remote_volumes.push(smolvm::remote_volume::from_parts(
                &mount.source,
                &mount.target,
                mount.read_only,
            )?);
        }

        let spec = MachineSpec {
            name: self.name,
            mounts,
            ports: self
                .ports
                .into_iter()
                .map(|p| PortMapping::new(p.host, p.guest))
                .collect(),
            resources: self.resources.to_vm_resources(),
            image: self.image,
            command: self.command,
            allowed_hosts: self.allowed_hosts,
            persistent: self.persistent,
            forkable: self.branchable,
            labels: self.labels.into_iter().collect(),
            runtime_managed: false,
            remote_volumes,
        };

        Ok(CreateRequest {
            spec,
            env: self.env,
            workdir: self.workdir,
            user: self.user,
        })
    }
}

/// A config translated into what the engine's create call wants.
#[derive(Debug)]
pub(crate) struct CreateRequest {
    pub(crate) spec: MachineSpec,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) workdir: Option<String>,
    pub(crate) user: Option<String>,
}

impl MachineConfig {
    /// Translate into the control plane's create request.
    ///
    /// Two things are rejected rather than silently dropped. A cloud machine
    /// has to come from an image, because there is no local rootfs to fall back
    /// on. And a host bind-mount has no meaning on a machine with no access to
    /// your filesystem — the API has no field for one, so sending the config
    /// anyway would quietly produce a machine missing the data it was supposed
    /// to have.
    pub(crate) fn into_cloud_request(self) -> Result<smol_cloud::types::CreateMachine> {
        use smol_cloud::types as wire;

        let image = self.image.ok_or_else(|| {
            Error::new(
                ErrorKind::Config,
                "a cloud machine needs an image; pass .image(\"…\") to the builder",
            )
        })?;
        if !self.mounts.is_empty() {
            return Err(Error::new(
                ErrorKind::NotSupported,
                "host mounts are local-only and are not applied on the cloud target; \
                 use cloud volumes for persistent storage instead",
            ));
        }

        let network = if !self.allowed_cidrs.is_empty() || !self.allowed_hosts.is_empty() {
            Some(wire::Network {
                mode: Some("allowCidrs".to_string()),
                cidrs: self.allowed_cidrs,
                hosts: self.allowed_hosts,
            })
        } else if self.resources.network.unwrap_or(false) {
            Some(wire::Network {
                mode: Some("open".to_string()),
                cidrs: Vec::new(),
                hosts: Vec::new(),
            })
        } else {
            None
        };

        Ok(wire::CreateMachine {
            name: Some(self.name),
            source: Some(wire::Source {
                source_type: "image".to_string(),
                reference: Some(image),
            }),
            resources: Some(wire::Resources {
                cpus: self.resources.cpus.map(u32::from),
                memory_mb: self.resources.memory_mib,
                disk_gb: self.resources.storage_gib.map(|gib| gib as u32),
            }),
            network,
            // Supply only the guest port: the control plane allocates the node
            // host port, so a host port chosen here would be ignored.
            ports: self
                .ports
                .into_iter()
                .map(|port| wire::Port {
                    port: port.guest,
                    host_port: None,
                })
                .collect(),
            env: (!self.env.is_empty()).then(|| self.env.into_iter().collect()),
            workdir: self.workdir,
            auto_stop_seconds: self.auto_stop_seconds,
            ttl_seconds: self.ttl_seconds,
            forkable: self.branchable,
        })
    }
}

/// Fluent front end for [`MachineConfig`], returned by
/// [`crate::Machine::builder`].
#[derive(Debug, Clone)]
pub struct MachineBuilder {
    config: MachineConfig,
}

impl MachineBuilder {
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self {
            config: MachineConfig::new(name),
        }
    }

    /// Boot this OCI image.
    pub fn image(mut self, image: impl Into<String>) -> Self {
        self.config.image = Some(image.into());
        self
    }

    /// Override the image entrypoint.
    pub fn command<I, S>(mut self, command: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config.command = command.into_iter().map(Into::into).collect();
        self
    }

    /// Set one environment variable for the workload.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.env.push((key.into(), value.into()));
        self
    }

    /// Working directory for the workload.
    pub fn workdir(mut self, workdir: impl Into<String>) -> Self {
        self.config.workdir = Some(workdir.into());
        self
    }

    /// User the workload runs as.
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.config.user = Some(user.into());
        self
    }

    /// Expose a directory in the guest.
    pub fn mount(mut self, mount: Mount) -> Self {
        self.config.mounts.push(mount);
        self
    }

    /// Forward a host port into the guest.
    pub fn port(mut self, port: Port) -> Self {
        self.config.ports.push(port);
        self
    }

    /// Virtual CPUs.
    pub fn cpus(mut self, cpus: u8) -> Self {
        self.config.resources.cpus = Some(cpus);
        self
    }

    /// Guest RAM in MiB.
    pub fn memory_mib(mut self, memory_mib: u32) -> Self {
        self.config.resources.memory_mib = Some(memory_mib);
        self
    }

    /// Give the guest outbound networking.
    pub fn network(mut self, network: bool) -> Self {
        self.config.resources.network = Some(network);
        self
    }

    /// Size the persistent data disk, in GiB.
    pub fn storage_gib(mut self, storage_gib: u64) -> Self {
        self.config.resources.storage_gib = Some(storage_gib);
        self
    }

    /// Size the image overlay, in GiB.
    pub fn overlay_gib(mut self, overlay_gib: u64) -> Self {
        self.config.resources.overlay_gib = Some(overlay_gib);
        self
    }

    /// Expose a GPU to the guest.
    pub fn gpu(mut self, gpu: bool) -> Self {
        self.config.resources.gpu = Some(gpu);
        self
    }

    /// GPU video memory in MiB.
    pub fn gpu_vram_mib(mut self, vram_mib: u32) -> Self {
        self.config.resources.gpu_vram_mib = Some(vram_mib);
        self
    }

    /// Expose CUDA via API remoting.
    pub fn cuda(mut self, cuda: bool) -> Self {
        self.config.resources.cuda = Some(cuda);
        self
    }

    /// Replace the whole resource block.
    pub fn resources(mut self, resources: Resources) -> Self {
        self.config.resources = resources;
        self
    }

    /// Attach caller metadata. The engine never interprets it, but it is the
    /// only way to recognise your own machines after the creating process dies.
    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.labels.push((key.into(), value.into()));
        self
    }

    /// Permit a hostname through the egress filter.
    pub fn allow_host(mut self, host: impl Into<String>) -> Self {
        self.config.allowed_hosts.push(host.into());
        self
    }

    /// Keep the machine's disks across stop and start.
    pub fn persistent(mut self, persistent: bool) -> Self {
        self.config.persistent = persistent;
        self
    }

    /// Back guest RAM with a memfd so the machine can be branched.
    pub fn branchable(mut self, branchable: bool) -> Self {
        self.config.branchable = branchable;
        self
    }

    /// Permit a CIDR through the egress filter. Cloud only.
    pub fn allow_cidr(mut self, cidr: impl Into<String>) -> Self {
        self.config.allowed_cidrs.push(cidr.into());
        self
    }

    /// Stop the machine after this many idle seconds. Cloud only.
    ///
    /// Worth setting on anything disposable: without it an idle machine runs,
    /// and bills, until something stops it.
    pub fn auto_stop_seconds(mut self, seconds: u64) -> Self {
        self.config.auto_stop_seconds = Some(seconds);
        self
    }

    /// Delete the machine after this many seconds, idle or not. Cloud only.
    pub fn ttl_seconds(mut self, seconds: u64) -> Self {
        self.config.ttl_seconds = Some(seconds);
        self
    }

    /// The config assembled so far.
    pub fn build(self) -> MachineConfig {
        self.config
    }

    /// Create the machine locally. It is not started yet.
    pub fn create(self) -> Result<crate::Machine> {
        crate::Machine::create(self.config)
    }

    /// Create the machine on the target these options select.
    pub fn create_with(self, connect: &crate::ConnectOptions) -> Result<crate::Machine> {
        crate::Machine::create_with(self.config, connect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_s3_source_becomes_a_remote_volume_not_a_host_mount() {
        let request = MachineConfig {
            name: "remote".into(),
            mounts: vec![Mount::new("s3://bucket/prefix", "/data")],
            ..Default::default()
        }
        .into_request()
        .expect("s3 sources bypass the host-directory check");

        assert!(request.spec.mounts.is_empty());
        assert_eq!(request.spec.remote_volumes.len(), 1);
    }

    #[test]
    fn a_staged_mount_cannot_also_be_read_only() {
        let error = MachineConfig {
            name: "conflict".into(),
            mounts: vec![Mount::new("/tmp", "/data").staged().read_only()],
            ..Default::default()
        }
        .into_request()
        .expect_err("staged writes back, so read-only is a contradiction");

        assert_eq!(error.kind(), ErrorKind::Mount);
    }

    #[test]
    fn a_remote_source_cannot_be_staged() {
        let error = MachineConfig {
            name: "staged-remote".into(),
            mounts: vec![Mount::new("s3://bucket/prefix", "/data").staged()],
            ..Default::default()
        }
        .into_request()
        .expect_err("staging copies from a host directory, which s3 is not");

        assert_eq!(error.kind(), ErrorKind::Mount);
    }

    #[test]
    fn unset_resources_fall_back_to_the_engine_defaults() {
        let resources = Resources::default().to_vm_resources();
        assert_eq!(resources.cpus, DEFAULT_MICROVM_CPU_COUNT);
        assert_eq!(resources.memory_mib, DEFAULT_MICROVM_MEMORY_MIB);
        assert!(!resources.network);
    }

    #[test]
    fn the_builder_carries_every_field_into_the_spec() {
        let request = built_config()
            .into_request()
            .expect("a config with no mounts always translates");

        assert_eq!(request.spec.name, "built");
        assert_eq!(request.spec.image.as_deref(), Some("alpine:latest"));
        assert_eq!(request.spec.resources.cpus, 4);
        assert_eq!(request.spec.resources.memory_mib, 2048);
        assert_eq!(request.spec.ports.len(), 1);
        assert!(request.spec.forkable);
        assert!(request.spec.persistent);
        assert_eq!(
            request.spec.labels.get("team").map(String::as_str),
            Some("qa")
        );
        assert_eq!(request.spec.allowed_hosts, vec!["example.com".to_string()]);
        assert_eq!(
            request.spec.command,
            vec!["sleep".to_string(), "infinity".to_string()]
        );
        assert_eq!(request.env, vec![("KEY".to_string(), "value".to_string())]);
        assert_eq!(request.workdir.as_deref(), Some("/work"));
        assert_eq!(request.user.as_deref(), Some("nobody"));
    }

    fn built_config() -> MachineConfig {
        MachineBuilder::new("built")
            .image("alpine:latest")
            .command(["sleep", "infinity"])
            .env("KEY", "value")
            .workdir("/work")
            .user("nobody")
            .port(Port::same(8080))
            .cpus(4)
            .memory_mib(2048)
            .label("team", "qa")
            .allow_host("example.com")
            .persistent(true)
            .branchable(true)
            .build()
    }
}
