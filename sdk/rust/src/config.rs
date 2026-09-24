//! Describing a machine before it exists.
//!
//! [`MachineConfig`] is a plain struct with a builder in front of it, so simple
//! cases stay one chained expression and complex ones can still be assembled
//! field by field and reused.

use crate::error::{Error, ErrorKind, Result};

/// A credential kept outside the machine. See [`MachineBuilder::credential`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    /// Name of the binding.
    pub name: String,
    /// The environment variable the workload reads (and that holds the real
    /// value on the host).
    pub env_var: String,
    /// Hosts the real value may be sent to.
    pub hosts: Vec<String>,
}

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
        // The engine treats these schemes as agent-fetched volumes.
        let source = self.source.as_str();
        source.starts_with("s3://")
            || source.starts_with("gs://")
            || source.starts_with("r2://")
            || source.starts_with("rclone://")
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

/// Everything needed to create a machine.
#[derive(Debug, Clone, Default)]
pub struct MachineConfig {
    /// Cloud creation waits for published services by default. Set false to
    /// wait only for guest exec, then install or start the service yourself.
    /// Local creation remains stopped until `start()`.
    pub wait_for_ports: Option<bool>,
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
    /// Credentials substituted on the way out: the guest sees a placeholder, and
    /// the engine swaps in the real value on requests to the named hosts.
    pub credentials: Vec<Credential>,
    /// Keep the machine's disks across stop and start.
    pub persistent: bool,
    /// Back guest RAM with a memfd on every start, so the machine can be
    /// branched.
    pub branchable: bool,
    /// CIDRs the machine may reach, on the local and cloud targets alike.
    pub allowed_cidrs: Vec<String>,
    /// Idle seconds before the machine stops on its own. Cloud only.
    pub auto_stop_seconds: Option<u64>,
    /// Hard lifetime, after which the machine is deleted. Cloud only.
    pub ttl_seconds: Option<u64>,
    /// CPU architecture to run on, `arm64` or `amd64`.
    ///
    /// On the cloud this places the machine. Locally there is only the host's
    /// architecture, so asking for a different one is rejected rather than
    /// quietly ignored.
    pub arch: Option<String>,
}

/// The canonical name for an architecture, so `aarch64` and `arm64` — or
/// `x86_64` and `amd64` — are not treated as two different places.
fn canonical_arch(arch: &str) -> &str {
    match arch {
        "aarch64" | "arm64" => "arm64",
        "x86_64" | "amd64" | "x86-64" => "amd64",
        other => other,
    }
}

impl MachineConfig {
    /// Start a config for a machine of this name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    /// Translate into `smolvm machine create` arguments.
    ///
    /// The SDK drives the installed CLI rather than linking the engine, so a
    /// local machine is described in the same flags a person would type. That
    /// is what keeps this crate publishable.
    pub(crate) fn into_local_args(self) -> Result<(Vec<String>, Vec<Port>)> {
        if let Some(arch) = &self.arch {
            let host = canonical_arch(std::env::consts::ARCH);
            if canonical_arch(arch) != host {
                return Err(Error::new(
                    ErrorKind::Config,
                    format!(
                        "this machine runs on the host, which is {host}, so it cannot be {arch}; \
                         drop .arch(), or create it on the cloud where placement is a choice"
                    ),
                ));
            }
        }

        let mut args: Vec<String> = vec![
            "machine".into(),
            "create".into(),
            "--name".into(),
            self.name.clone(),
        ];
        if let Some(image) = &self.image {
            args.push("--image".into());
            args.push(image.clone());
        }
        if let Some(cpus) = self.resources.cpus {
            args.push("--cpus".into());
            args.push(cpus.to_string());
        }
        if let Some(memory) = self.resources.memory_mib {
            args.push("--mem".into());
            args.push(memory.to_string());
        }
        if self.resources.network.unwrap_or(false) {
            args.push("--net".into());
        }
        if let Some(storage) = self.resources.storage_gib {
            args.push("--storage".into());
            args.push(storage.to_string());
        }
        if self.resources.gpu.unwrap_or(false) {
            args.push("--gpu".into());
        }
        for host in &self.allowed_hosts {
            args.push("--allow-host".into());
            args.push(host.clone());
        }
        for credential in &self.credentials {
            args.push("--credential".into());
            args.push(format!(
                "{}={}@{}",
                credential.name,
                credential.env_var,
                credential.hosts.join(",")
            ));
        }
        for cidr in &self.allowed_cidrs {
            args.push("--allow-cidr".into());
            args.push(cidr.clone());
        }
        for port in &self.ports {
            args.push("-p".into());
            args.push(format!("{}:{}", port.host, port.guest));
        }
        for mount in &self.mounts {
            if mount.read_only && mount.staged {
                return Err(Error::new(
                    ErrorKind::Mount,
                    "a staged mount is writable and cannot also be read-only",
                ));
            }
            // One flag for every kind of volume, with the mode as a suffix:
            // `-v HOST|REMOTE:GUEST[:ro|rw|staged]`. The CLI takes s3 sources
            // here too, so a remote volume needs no special case.
            let mode = if mount.staged {
                ":staged"
            } else if mount.read_only {
                ":ro"
            } else {
                ""
            };
            args.push("-v".into());
            args.push(format!("{}:{}{mode}", mount.source, mount.target));
        }
        for (key, value) in &self.env {
            args.push("--env".into());
            args.push(format!("{key}={value}"));
        }
        if let Some(workdir) = &self.workdir {
            args.push("--workdir".into());
            args.push(workdir.clone());
        }
        if let Some(user) = &self.user {
            args.push("--user".into());
            args.push(user.clone());
        }
        for (key, value) in &self.labels {
            args.push("--label".into());
            args.push(format!("{key}={value}"));
        }
        if !self.command.is_empty() {
            args.push("--".into());
            args.extend(self.command.iter().cloned());
        }
        Ok((args, self.ports))
    }
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
        if self.mounts.iter().any(|mount| !mount.is_remote()) {
            return Err(Error::new(
                ErrorKind::NotSupported,
                "host mounts are local-only and are not applied on the cloud target; \
                 use cloud volumes for persistent storage instead",
            ));
        }
        if !self.credentials.is_empty() {
            return Err(Error::new(
                ErrorKind::NotSupported,
                "credential substitution runs in the local engine; the cloud target \
                 cannot keep a credential outside the machine yet",
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
                arch: self.arch,
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
            command: self.command,
            auto_stop_seconds: self.auto_stop_seconds,
            ttl_seconds: self.ttl_seconds,
            branchable: self.branchable,
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

    /// Whether cloud creation waits for published services (default true).
    pub fn wait_for_ports(mut self, wait: bool) -> Self {
        self.config.wait_for_ports = Some(wait);
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

    /// Keep a credential out of the machine: the guest's `env_var` holds a
    /// placeholder, and the engine substitutes the real value — read from the
    /// same variable in the environment the machine starts from — only on
    /// requests to `hosts`. Local target only.
    pub fn credential<I, H>(
        mut self,
        name: impl Into<String>,
        env_var: impl Into<String>,
        hosts: I,
    ) -> Self
    where
        I: IntoIterator<Item = H>,
        H: Into<String>,
    {
        self.config.credentials.push(Credential {
            name: name.into(),
            env_var: env_var.into(),
            hosts: hosts.into_iter().map(Into::into).collect(),
        });
        self
    }

    /// Permit a CIDR through the egress filter.
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

    /// Run on this CPU architecture, `arm64` or `amd64`.
    ///
    /// Worth setting when something downstream cares which one you got — a
    /// checkpoint only restores on the architecture it was taken on. Leaving it
    /// unset lets the cloud place the machine wherever it has room.
    pub fn arch(mut self, arch: impl Into<String>) -> Self {
        self.config.arch = Some(arch.into());
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
    fn every_volume_goes_through_one_flag_with_its_mode_as_a_suffix() {
        let (args, _) = MachineConfig {
            name: "m".into(),
            mounts: vec![
                Mount::new("/src", "/work"),
                Mount::new("/data", "/ro").read_only(),
                Mount::new("/cache", "/staged").staged(),
                Mount::new("s3://bucket/prefix", "/models"),
            ],
            ..Default::default()
        }
        .into_local_args()
        .expect("the CLI takes host and remote volumes through the same flag");
        let joined = args.join(" ");
        assert!(joined.contains("-v /src:/work"), "{joined}");
        assert!(joined.contains("-v /data:/ro:ro"), "{joined}");
        assert!(joined.contains("-v /cache:/staged:staged"), "{joined}");
        assert!(joined.contains("-v s3://bucket/prefix:/models"), "{joined}");
        // `--mount` does not exist; emitting it made every mounted machine fail.
        assert!(!joined.contains("--mount"), "{joined}");
    }

    #[test]
    fn a_staged_mount_cannot_also_be_read_only() {
        let error = MachineConfig {
            name: "conflict".into(),
            mounts: vec![Mount::new("/tmp", "/data").staged().read_only()],
            ..Default::default()
        }
        .into_local_args()
        .expect_err("staged writes back, so read-only is a contradiction");

        assert_eq!(error.kind(), ErrorKind::Mount);
    }

    #[test]
    fn the_builder_carries_every_field_into_the_spec() {
        let (args, ports) = built_config()
            .into_local_args()
            .expect("a config with no mounts always translates");
        let joined = args.join(" ");

        assert!(joined.contains("--name built"), "{joined}");
        assert!(joined.contains("--image alpine:latest"), "{joined}");
        assert!(joined.contains("--cpus 4"), "{joined}");
        assert!(joined.contains("--mem 2048"), "{joined}");
        assert!(joined.contains("--env KEY=value"), "{joined}");
        assert!(joined.contains("--workdir /work"), "{joined}");
        assert!(joined.contains("--user nobody"), "{joined}");
        assert!(joined.contains("--label team=qa"), "{joined}");
        assert!(joined.contains("--allow-host example.com"), "{joined}");
        assert!(joined.contains("-- sleep infinity"), "{joined}");
        assert_eq!(ports.len(), 1);
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
