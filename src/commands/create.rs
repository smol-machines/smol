//! smol machine create — create a persistent machine.

use clap::Args;
use smolvm::config::SmolvmConfig;
use smolvm::data::network::PortMapping;
use smolvm::network::NetworkBackend;

#[derive(Args, Debug)]
pub struct CreateCmd {
    /// Machine name (auto-generated if omitted)
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// Container image
    #[arg(short = 'I', long, value_name = "IMAGE", conflicts_with = "from")]
    pub image: Option<String>,

    /// Create from a packed .smolmachine artifact (uses its pre-extracted layers
    /// instead of pulling from a registry)
    #[arg(long, value_name = "PATH")]
    pub from: Option<std::path::PathBuf>,

    /// Number of vCPUs
    #[arg(long, default_value_t = 4, value_name = "N")]
    pub cpus: u8,

    /// Memory in MiB
    #[arg(long, default_value_t = 8192, value_name = "MiB")]
    pub mem: u32,

    /// Storage disk size in GiB (OCI layers and container data)
    #[arg(long, value_name = "GiB")]
    pub storage: Option<u64>,

    /// Overlay disk size in GiB (persistent rootfs changes)
    #[arg(long, value_name = "GiB")]
    pub overlay: Option<u64>,

    /// Enable GPU acceleration (Vulkan via virtio-gpu)
    #[arg(long)]
    pub gpu: bool,

    /// GPU VRAM in MiB (requires --gpu)
    #[arg(long, value_name = "MiB")]
    pub gpu_vram: Option<u32>,

    /// Remote guest CUDA Driver-API calls to the host NVIDIA GPU over vsock
    #[arg(long, help_heading = "Hardware")]
    pub cuda: bool,

    /// Mount host directory (HOST:GUEST[:ro])
    #[arg(short = 'v', long = "volume", value_name = "HOST:GUEST[:ro]")]
    pub volume: Vec<String>,

    /// Expose port (HOST:GUEST)
    #[arg(short = 'p', long = "port", value_parser = PortMapping::parse, value_name = "HOST:GUEST")]
    pub port: Vec<PortMapping>,

    /// Enable outbound network access
    #[arg(long)]
    pub net: bool,

    /// Allow egress only to this CIDR (repeatable; implies --net). e.g. 10.0.0.0/8
    #[arg(long = "allow-cidr", value_name = "CIDR")]
    pub allow_cidr: Vec<String>,

    /// Allow egress only to this hostname's IPs, resolved at start (repeatable;
    /// implies --net)
    #[arg(long = "allow-host", value_name = "HOSTNAME")]
    pub allow_host: Vec<String>,

    /// Restrict egress to localhost only (implies --net)
    #[arg(long)]
    pub outbound_localhost_only: bool,

    /// Network backend (tsi or virtio-net)
    #[arg(long = "net-backend", value_enum, value_name = "BACKEND")]
    pub net_backend: Option<NetworkBackend>,

    /// Forward the host SSH agent into the machine
    #[arg(long)]
    pub ssh_agent: bool,

    /// Command to run once on each start, before the workload (repeatable)
    #[arg(long = "init", value_name = "COMMAND")]
    pub init: Vec<String>,

    /// Set environment variable (KEY=VALUE)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Set working directory
    #[arg(short = 'w', long, value_name = "DIR")]
    pub workdir: Option<String>,

    /// Inject a secret from a host env var (GUEST_VAR=HOST_VAR), resolved at
    /// each start/exec; only the reference is stored, never the plaintext
    #[arg(long = "secret-env", value_name = "GUEST_VAR=HOST_VAR")]
    pub secret_env: Vec<String>,

    /// Inject a secret from a host file (GUEST_VAR=/abs/path), resolved at each
    /// start/exec; only the reference is stored, never the plaintext
    #[arg(long = "secret-file", value_name = "GUEST_VAR=PATH")]
    pub secret_file: Vec<String>,
}

impl CreateCmd {
    pub fn run(self) -> anyhow::Result<()> {
        if let Some(from) = self.from.clone() {
            return self.create_from_smolmachine(&from);
        }

        let name = self
            .name
            .unwrap_or_else(smolvm::util::generate_machine_name);

        let mounts: Vec<(String, String, bool)> = smolvm::data::storage::HostMount::parse(&self.volume)?
            .into_iter()
            .map(|m| m.to_storage_tuple())
            .collect();

        let ports = smolvm::data::network::PortMapping::to_tuples(&self.port);

        let env = smolvm::util::parse_env_list(&self.env);

        // Egress policy: validate --allow-cidr, resolve --allow-host to CIDRs,
        // add localhost CIDRs for --outbound-localhost-only. Any of these implies
        // outbound networking. Hosts are re-resolved at start (IPs rotate).
        let mut allow_cidrs: Vec<String> = Vec::new();
        for c in &self.allow_cidr {
            allow_cidrs.push(
                smolvm::smolfile::parse_cidr(c).map_err(|e| anyhow::anyhow!("--allow-cidr: {e}"))?,
            );
        }
        for h in &self.allow_host {
            let cidrs = smolvm::smolfile::resolve_host_to_cidrs(h)
                .map_err(|e| anyhow::anyhow!("--allow-host: {e}"))?;
            allow_cidrs.extend(cidrs);
        }
        if self.outbound_localhost_only {
            allow_cidrs.push("127.0.0.0/8".to_string());
            allow_cidrs.push("::1/128".to_string());
        }
        let net = self.net || !allow_cidrs.is_empty();

        let mut record = smolvm::config::VmRecord::new(
            name.clone(),
            self.cpus,
            self.mem,
            mounts,
            ports,
            net,
        );
        if !allow_cidrs.is_empty() {
            record.allowed_cidrs = Some(allow_cidrs);
        }
        if !self.allow_host.is_empty() {
            record.dns_filter_hosts = Some(self.allow_host.clone());
        }
        record.image = self.image;
        record.env = env;
        record.workdir = self.workdir;
        record.init = self.init;
        record.ssh_agent = self.ssh_agent;
        // Store secret references (not plaintext); resolved at each start/exec.
        record.secret_refs = super::common::parse_cli_secret_refs(&self.secret_env, &self.secret_file)?;
        record.network_backend = self.net_backend;
        record.storage_gb = self.storage;
        record.overlay_gb = self.overlay;
        // Match the engine: store gpu only when enabled (None == off), and
        // reject a zero VRAM value rather than silently accepting it.
        record.gpu = if self.gpu { Some(true) } else { None };
        record.gpu_vram_mib = smolvm::data::resources::validate_gpu_vram_mib(self.gpu_vram)
            .map_err(|e| anyhow::anyhow!("--gpu-vram: {}", e))?;
        record.cuda = self.cuda;

        let mut config = SmolvmConfig::load()?;
        config.insert_vm(name.clone(), record)?;

        println!("Created machine: {}", name);
        println!("  CPUs: {}, Memory: {} MiB", self.cpus, self.mem);
        println!("\nUse 'smol machine start --name {}' to start the machine", name);
        Ok(())
    }

    /// Create a machine from a packed `.smolmachine`: read its manifest for the
    /// config defaults, extract its pre-extracted layers into the machine's
    /// cache, and store `source_smolmachine` so start uses them instead of
    /// pulling. Port of the engine's `run_from_smolmachine`.
    fn create_from_smolmachine(self, sidecar_path: &std::path::Path) -> anyhow::Result<()> {
        if !sidecar_path.exists() {
            anyhow::bail!("file not found: {}", sidecar_path.display());
        }
        let manifest = smolvm_pack::packer::read_manifest_from_sidecar(sidecar_path)
            .map_err(|e| anyhow::anyhow!("read .smolmachine: {e}"))?;
        let footer = smolvm_pack::packer::read_footer_from_sidecar(sidecar_path)
            .map_err(|e| anyhow::anyhow!("read sidecar footer: {e}"))?;
        let canonical = sidecar_path
            .canonicalize()
            .unwrap_or_else(|_| sidecar_path.to_path_buf())
            .to_string_lossy()
            .into_owned();

        let name = self
            .name
            .clone()
            .unwrap_or_else(smolvm::util::generate_machine_name);

        // CLI flags override manifest defaults (defaults are 4 cpus / 8192 MiB).
        let cpus = if self.cpus != 4 { self.cpus } else { manifest.cpus };
        let mem = if self.mem != 8192 { self.mem } else { manifest.mem };

        // A .smolmachine is an untrusted, portable artifact: reject any secret
        // refs it carries (Untrusted scope rejects every source kind) so a packed
        // `from_env`/`from_file` can't read THIS host's env/files at exec time.
        for (key, r) in &manifest.secret_refs {
            smolvm::secrets::validate_ref(r, smolvm::secrets::ResolutionScope::Untrusted)
                .map_err(|e| {
                    anyhow::anyhow!("create from .smolmachine: secret '{key}': {e} (packs may not carry secret refs)")
                })?;
        }

        let mounts: Vec<(String, String, bool)> = smolvm::data::storage::HostMount::parse(&self.volume)?
            .into_iter()
            .map(|m| m.to_storage_tuple())
            .collect();
        let ports = smolvm::data::network::PortMapping::to_tuples(&self.port);
        let mut env = smolvm::util::parse_env_list(&manifest.env);
        env.extend(smolvm::util::parse_env_list(&self.env));

        let mut record = smolvm::config::VmRecord::new(
            name.clone(),
            cpus,
            mem,
            mounts,
            ports,
            self.net || manifest.network,
        );
        record.image = Some(manifest.image);
        record.entrypoint = manifest.entrypoint;
        record.cmd = manifest.cmd;
        record.env = env;
        record.workdir = manifest.workdir;
        record.init = self.init;
        record.ssh_agent = self.ssh_agent;
        record.network_backend = self.net_backend;
        record.storage_gb = self.storage;
        record.overlay_gb = self.overlay;
        record.gpu = if manifest.gpu { Some(true) } else { None };
        record.source_smolmachine = Some(canonical);

        // Create the data dir, then extract the bundle's layers into the
        // machine's cache. Detach the layers volume on both success and failure
        // (macOS mounts the case-sensitive volume even when extraction fails).
        let _manager = smolvm::agent::AgentManager::for_vm_with_sizes(
            &name,
            record.storage_gb,
            record.overlay_gb,
        )?;
        let cache_dir = smolvm::agent::machine_layers_cache_dir(&name);
        smolvm_pack::extract::force_detach_layers_volume(&cache_dir);
        match std::fs::remove_dir_all(&cache_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => anyhow::bail!("clear packed layers cache: {e}"),
        }
        println!("Extracting .smolmachine assets...");
        let result = smolvm_pack::extract::extract_sidecar(sidecar_path, &cache_dir, &footer, false, false)
            .map_err(|e| anyhow::anyhow!("extract sidecar: {e}"));
        smolvm_pack::extract::force_detach_layers_volume(&cache_dir);
        result?;

        let mut config = SmolvmConfig::load()?;
        config.insert_vm(name.clone(), record)?;

        println!("Created machine: {name} (from {})", sidecar_path.display());
        println!("  CPUs: {cpus}, Memory: {mem} MiB");
        println!("\nUse 'smol machine start --name {name}' to start the machine");
        Ok(())
    }
}
