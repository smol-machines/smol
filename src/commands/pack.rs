//! smol pack — build portable .smolmachine artifacts.

use clap::{Args, Subcommand};
use smolvm::agent::{AgentClient, AgentManager, PullOptions, VmResources};
use smolvm::config::{RecordState, SmolvmConfig};
use smolvm::data::resources::DEFAULT_MICROVM_CPU_COUNT;
use smolvm::smolfile;
use smolvm_pack::assets::AssetCollector;
use smolvm_pack::format::{PackManifest, PackMode};
use smolvm_pack::packer::Packer;
use smolvm_protocol::AgentResponse;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Default memory for packed VMs (lower than machine create).
const PACK_DEFAULT_MEMORY_MIB: u32 = 256;

#[derive(Subcommand, Debug)]
pub enum PackCmd {
    /// Package an OCI image or VM snapshot into a portable executable
    Create(PackCreateCmd),

    /// Push a .smolmachine artifact to a registry
    Push(crate::commands::push::PushCmd),

    /// Pull a .smolmachine artifact from a registry
    Pull(crate::commands::pull::PullCmd),

    /// Inspect a .smolmachine artifact in a registry
    Inspect(crate::commands::inspect::InspectCmd),
}

impl PackCmd {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            PackCmd::Create(cmd) => cmd.run(),
            PackCmd::Push(cmd) => cmd.run(),
            PackCmd::Pull(cmd) => cmd.run(),
            PackCmd::Inspect(cmd) => cmd.run(),
        }
    }
}

#[derive(Args, Debug)]
pub struct PackCreateCmd {
    /// Container image to pack (e.g., alpine, python:3.12-alpine)
    #[arg(
        long,
        short = 'I',
        value_name = "IMAGE",
        required_unless_present_any = ["from_vm", "smolfile"],
        conflicts_with = "from_vm"
    )]
    pub image: Option<String>,

    /// Pack from a stopped VM snapshot
    #[arg(long = "from-vm", value_name = "VM_NAME")]
    pub from_vm: Option<String>,

    /// Output file path
    #[arg(short = 'o', long, value_name = "PATH")]
    pub output: PathBuf,

    /// Default vCPUs for the packed VM
    #[arg(long, default_value_t = DEFAULT_MICROVM_CPU_COUNT, value_name = "N")]
    pub cpus: u8,

    /// Default memory in MiB
    #[arg(long, default_value_t = PACK_DEFAULT_MEMORY_MIB, value_name = "MiB")]
    pub mem: u32,

    /// Target OCI platform (e.g., linux/arm64)
    #[arg(long = "oci-platform", value_name = "OS/ARCH")]
    pub oci_platform: Option<String>,

    /// Override image entrypoint
    #[arg(long, value_name = "CMD")]
    pub entrypoint: Option<String>,

    /// Skip code signing (macOS only)
    #[arg(long)]
    pub no_sign: bool,

    /// Single file (no sidecar)
    #[arg(long)]
    pub single_file: bool,

    /// Also produce a runnable executable launcher alongside the portable
    /// artifact. By default only the portable (deployable) artifact is written —
    /// pass this when you want to run the packed image locally.
    #[arg(long)]
    pub launcher: bool,

    /// Path to stub binary
    #[arg(long, value_name = "PATH", hide = true)]
    pub stub: Option<PathBuf>,

    /// Path to library directory
    #[arg(long, value_name = "DIR", hide = true)]
    pub lib_dir: Option<PathBuf>,

    /// Path to agent rootfs
    #[arg(long, value_name = "DIR", hide = true)]
    pub rootfs_dir: Option<PathBuf>,

    /// Load config from Smolfile
    #[arg(long = "smolfile", short = 's', value_name = "PATH")]
    pub smolfile: Option<PathBuf>,
}

impl PackCreateCmd {
    pub fn run(self) -> anyhow::Result<()> {
        if let Some(vm_name) = self.from_vm.clone() {
            return self.pack_from_vm(vm_name);
        }

        // Resolve config from Smolfile + CLI
        let pack_config = self.resolve_pack_config()?;

        let image = pack_config.image.clone().ok_or_else(|| {
            anyhow::anyhow!("no image specified. Use --image or set 'image' in Smolfile")
        })?;

        let temp_dir = tempfile::tempdir()?;
        let staging_dir = temp_dir.path().join("staging");

        // Start temporary VM for image pulling. The name must start with a
        // letter/digit (validate_vm_name) — a leading `_` is rejected — so use a
        // `pack-` prefix rather than `__pack_`.
        let pack_vm_name = format!(
            "pack-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );

        println!("Starting agent VM...");
        let manager = AgentManager::for_vm(&pack_vm_name)?;
        manager.start_with_config(
            Vec::new(),
            VmResources {
                cpus: 2,
                memory_mib: 512,
                network: true,
                storage_gib: None,
                overlay_gib: None,
                allowed_cidrs: None,
                network_backend: None,
                gpu: false,
                gpu_vram_mib: None,
                dns: None,
            },
        )?;

        // Ensure cleanup on any exit path
        let result = self.do_pack_image(&manager, &image, &pack_config, &staging_dir);

        // Always clean up the temporary VM
        if let Err(e) = manager.stop() {
            eprintln!("Warning: failed to stop pack VM: {}", e);
        }
        let vm_data = smolvm::agent::vm_data_dir(&pack_vm_name);
        let _ = std::fs::remove_dir_all(&vm_data);

        result
    }

    fn do_pack_image(
        &self,
        manager: &AgentManager,
        image: &str,
        pack_config: &PackConfig,
        staging_dir: &std::path::Path,
    ) -> anyhow::Result<()> {
        let mut client = manager.connect()?;

        // Pull image
        println!("Pulling {}...", image);
        let mut pull_opts = PullOptions::new().use_registry_config(true);
        if let Some(ref platform) = pack_config.oci_platform {
            pull_opts = pull_opts.oci_platform(platform);
        }
        let image_info = client.pull(image, pull_opts)?;

        println!(
            "Image: {} ({} layers, {} bytes)",
            image, image_info.layer_count, image_info.size
        );

        // Collect base assets
        let mut collector = AssetCollector::new(staging_dir.to_path_buf())
            .map_err(|e| anyhow::anyhow!("collect assets: {}", e))?;
        self.collect_base_assets(&mut collector)?;

        // Export layers
        println!("Exporting {} layers...", image_info.layer_count);
        for (i, layer_digest) in image_info.layers.iter().enumerate() {
            println!(
                "  Layer {}/{}: {}...",
                i + 1,
                image_info.layer_count,
                &layer_digest[..19.min(layer_digest.len())]
            );
            let layer_data = export_layer(&mut client, &image_info.digest, i)?;
            collector
                .add_layer(layer_digest, &layer_data)
                .map_err(|e| anyhow::anyhow!("collect layers: {}", e))?;
        }

        // Build manifest
        let platform = format!("{}/{}", image_info.os, image_info.architecture);
        let host_platform = smolvm::platform::Platform::current()
            .host_oci_platform()
            .to_string();
        let mut manifest = PackManifest::new(
            image.to_string(),
            image_info.digest.clone(),
            platform,
            host_platform,
        );
        manifest.image_size = image_info.size;
        manifest.cpus = pack_config.cpus;
        manifest.mem = pack_config.mem;
        manifest.entrypoint = image_info.entrypoint.clone();
        manifest.cmd = image_info.cmd.clone();
        manifest.env = image_info.env.clone();
        manifest.workdir = image_info.workdir.clone();

        // Layer Smolfile overrides
        apply_pack_overrides(&mut manifest, pack_config);

        self.finalize_pack(manifest, collector, staging_dir)
    }

    fn pack_from_vm(self, vm_name: String) -> anyhow::Result<()> {
        let config = SmolvmConfig::load()?;
        let vm = config
            .get_vm(&vm_name)
            .ok_or_else(|| anyhow::anyhow!("VM '{}' not found", vm_name))?;

        if vm.actual_state() == RecordState::Running {
            anyhow::bail!(
                "VM '{}' is running. Stop it first: smol stop {}",
                vm_name,
                vm_name
            );
        }

        let overlay_path = smolvm::agent::vm_data_dir(&vm_name).join("overlay.raw");
        if !overlay_path.exists() {
            anyhow::bail!("overlay disk not found. The VM may not have been started yet.");
        }

        println!("Packing VM '{}' snapshot...", vm_name);

        let temp_dir = tempfile::tempdir()?;
        let staging_dir = temp_dir.path().join("staging");

        let mut collector = AssetCollector::new(staging_dir.clone())
            .map_err(|e| anyhow::anyhow!("collect assets: {}", e))?;
        self.collect_base_assets(&mut collector)?;

        println!("Copying overlay disk...");
        collector
            .add_overlay_template(&overlay_path)
            .map_err(|e| anyhow::anyhow!("collect overlay: {}", e))?;

        let pack_config = self.resolve_pack_config()?;

        let platform = format!("linux/{}", smolvm::platform::Arch::current().oci_arch());
        let host_platform = smolvm::platform::Platform::current()
            .host_oci_platform()
            .to_string();
        let mut manifest = PackManifest::new(
            format!("vm://{}", vm_name),
            "none".to_string(),
            platform,
            host_platform,
        );
        manifest.mode = PackMode::Vm;
        manifest.cpus = pack_config.cpus;
        manifest.mem = pack_config.mem;
        manifest.entrypoint = if !vm.entrypoint.is_empty() {
            vm.entrypoint.clone()
        } else {
            vec!["/bin/sh".to_string()]
        };
        manifest.cmd = vm.cmd.clone();
        manifest.env = vm.env.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
        manifest.workdir = vm.workdir.clone();

        apply_pack_overrides(&mut manifest, &pack_config);

        self.finalize_pack(manifest, collector, &staging_dir)
    }

    fn resolve_pack_config(&self) -> anyhow::Result<PackConfig> {
        let sf = match &self.smolfile {
            Some(path) => Some(smolfile::load(path)?),
            None => {
                // Auto-detect Smolfile in current directory
                let default = PathBuf::from("Smolfile");
                if default.exists() {
                    Some(smolfile::load(&default)?)
                } else {
                    None
                }
            }
        };

        let sf = match sf {
            Some(sf) => sf,
            None => {
                return Ok(PackConfig {
                    image: self.image.clone(),
                    entrypoint: self
                        .entrypoint
                        .as_ref()
                        .map(|e| vec![e.clone()])
                        .unwrap_or_default(),
                    cmd: vec![],
                    cpus: self.cpus,
                    mem: self.mem,
                    oci_platform: self.oci_platform.clone(),
                    env: vec![],
                    workdir: None,
                });
            }
        };

        let artifact = sf.artifact.or(sf.pack).unwrap_or_default();

        let image = self.image.clone().or(sf.image);

        let entrypoint = if let Some(ref ep) = self.entrypoint {
            vec![ep.clone()]
        } else if !artifact.entrypoint.is_empty() {
            artifact.entrypoint
        } else {
            sf.entrypoint
        };

        let cmd = if !artifact.cmd.is_empty() {
            artifact.cmd
        } else {
            sf.cmd
        };

        let cpus = if self.cpus != DEFAULT_MICROVM_CPU_COUNT {
            self.cpus
        } else {
            artifact.cpus.or(sf.cpus).unwrap_or(self.cpus)
        };

        let mem = if self.mem != PACK_DEFAULT_MEMORY_MIB {
            self.mem
        } else {
            artifact.memory.or(sf.memory).unwrap_or(self.mem)
        };

        let oci_platform = self.oci_platform.clone().or(artifact.oci_platform);

        Ok(PackConfig {
            image,
            entrypoint,
            cmd,
            cpus,
            mem,
            oci_platform,
            env: sf.env.into_iter().map(|e| e.trim().to_string()).collect(),
            workdir: sf.workdir,
        })
    }

    fn collect_base_assets(&self, collector: &mut AssetCollector) -> anyhow::Result<()> {
        println!("Collecting runtime libraries...");
        let lib_dir = self.find_lib_dir()?;
        collector
            .collect_libraries(&lib_dir)
            .map_err(|e| anyhow::anyhow!("collect libraries: {}", e))?;

        println!("Collecting agent rootfs...");
        let rootfs_dir = self.find_rootfs_dir()?;
        collector
            .collect_agent_rootfs(&rootfs_dir)
            .map_err(|e| anyhow::anyhow!("collect rootfs: {}", e))?;

        println!("Creating storage template...");
        collector
            .create_storage_template()
            .map_err(|e| anyhow::anyhow!("create storage template: {}", e))?;

        Ok(())
    }

    fn finalize_pack(
        &self,
        mut manifest: PackManifest,
        collector: AssetCollector,
        staging_dir: &std::path::Path,
    ) -> anyhow::Result<()> {
        let stub_path = self.find_smolvm_binary()?;

        manifest.assets = collector.into_inventory();

        let collector = AssetCollector::new(staging_dir.to_path_buf())
            .map_err(|e| anyhow::anyhow!("collect assets: {}", e))?;

        let packer = Packer::new(manifest)
            .with_stub(&stub_path)
            .with_asset_collector(collector);

        // `--single-file` and `--launcher` both produce a runnable executable.
        // The default produces ONLY the portable (deployable) artifact, so
        // there's no ambiguity about which file to `smol deploy -f`.
        let produce_launcher = self.single_file || self.launcher;

        let info = if self.single_file {
            println!("Assembling single-file packed binary...");
            packer
                .pack_embedded(&self.output)
                .map_err(|e| anyhow::anyhow!("pack binary: {}", e))?
        } else {
            println!("Assembling portable artifact...");
            packer
                .pack(&self.output)
                .map_err(|e| anyhow::anyhow!("pack binary: {}", e))?
        };

        if produce_launcher {
            println!(
                "Packed launcher: {} (stub: {}KB, total: {}KB)",
                self.output.display(),
                info.stub_size / 1024,
                info.total_size / 1024
            );

            // Sign on macOS
            if cfg!(target_os = "macos") && !self.no_sign {
                println!("Signing binary...");
                if let Err(e) =
                    smolvm_pack::signing::sign_with_hypervisor_entitlements(&self.output)
                {
                    eprintln!("Warning: signing failed: {}", e);
                }
            }

            // Embed libs after signing (single-file embeds them inline already).
            if !self.single_file {
                smolvm_pack::packer::embed_libs_in_binary(&self.output, staging_dir)
                    .map_err(|e| anyhow::anyhow!("embed libraries: {}", e))?;
            }

            println!("\nRun with: {}", self.output.display());
        } else {
            // Default: keep only the portable sidecar artifact. `pack` wrote the
            // executable stub to `output` and the sidecar to `output<ext>`;
            // replace the stub with the sidecar so `output` IS the deployable
            // artifact (the host/node supplies the runtime libs).
            let sidecar = smolvm_pack::packer::sidecar_path_for(&self.output);
            std::fs::rename(&sidecar, &self.output)
                .map_err(|e| anyhow::anyhow!("finalize portable artifact: {}", e))?;
            println!(
                "Packed: {} (portable artifact — `smol deploy -f` to run it remotely; \
                 pass --launcher to also build a runnable local executable)",
                self.output.display()
            );
        }

        Ok(())
    }

    fn find_lib_dir(&self) -> anyhow::Result<PathBuf> {
        if let Some(ref dir) = self.lib_dir {
            return Ok(dir.clone());
        }

        let platform_lib = format!("lib/linux-{}", std::env::consts::ARCH);
        let dylib_ext = if cfg!(target_os = "macos") {
            "dylib"
        } else {
            "so"
        };
        let lib_name = format!("libkrun.{}", dylib_ext);

        let candidates = [
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("lib"))),
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().and_then(|d| d.parent()).map(|d| d.join("lib"))),
            std::env::current_exe().ok().and_then(|p| {
                p.parent()
                    .and_then(|d| d.parent())
                    .map(|d| d.join(&platform_lib))
            }),
            Some(PathBuf::from("lib")),
            Some(PathBuf::from(&platform_lib)),
            Some(PathBuf::from("/opt/homebrew/lib")),
            Some(PathBuf::from("/usr/local/lib")),
        ];

        for candidate in candidates.into_iter().flatten() {
            if candidate.join(&lib_name).exists() {
                return Ok(candidate);
            }
        }

        anyhow::bail!("could not find libkrun. Use --lib-dir to specify the location.")
    }

    fn find_rootfs_dir(&self) -> anyhow::Result<PathBuf> {
        if let Some(ref dir) = self.rootfs_dir {
            return Ok(dir.clone());
        }

        let candidates = [
            std::env::var("SMOLVM_AGENT_ROOTFS").ok().map(PathBuf::from),
            dirs::data_dir().map(|d| d.join("smolvm/agent-rootfs")),
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("agent-rootfs"))),
        ];

        for candidate in candidates.into_iter().flatten() {
            if std::fs::symlink_metadata(candidate.join("sbin/init")).is_ok() {
                return Ok(candidate);
            }
        }

        anyhow::bail!("could not find agent rootfs. Use --rootfs-dir to specify the location.")
    }

    fn find_smolvm_binary(&self) -> anyhow::Result<PathBuf> {
        if let Some(ref path) = self.stub {
            return Ok(path.clone());
        }

        let candidates = [
            Some(PathBuf::from("target/release/smolvm")),
            Some(PathBuf::from("target/debug/smolvm")),
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("smolvm-bin"))),
            std::env::current_exe().ok(),
            dirs::data_dir().map(|d| d.join("smolvm/smolvm-bin")),
        ];

        for candidate in candidates.into_iter().flatten() {
            if candidate.exists() {
                return Ok(candidate);
            }
        }

        anyhow::bail!(
            "could not find smolvm binary. Build it with:\n  \
             cargo build --release\n\
             Or use --stub to specify the path."
        )
    }
}

/// Resolved pack configuration.
struct PackConfig {
    image: Option<String>,
    entrypoint: Vec<String>,
    cmd: Vec<String>,
    cpus: u8,
    mem: u32,
    oci_platform: Option<String>,
    env: Vec<String>,
    workdir: Option<String>,
}

/// Apply Smolfile/CLI overrides to the pack manifest.
fn apply_pack_overrides(manifest: &mut PackManifest, config: &PackConfig) {
    // Layer env overrides (dedup by key)
    for e in &config.env {
        if let Some((key, _)) = e.split_once('=') {
            manifest
                .env
                .retain(|existing| !existing.starts_with(&format!("{}=", key)));
        }
        manifest.env.push(e.clone());
    }

    if config.workdir.is_some() {
        manifest.workdir = config.workdir.clone();
    }
    if !config.entrypoint.is_empty() {
        manifest.entrypoint = config.entrypoint.clone();
    }
    if !config.cmd.is_empty() {
        manifest.cmd = config.cmd.clone();
    }
}

/// Export a layer from the agent via chunked streaming.
fn export_layer(
    client: &mut AgentClient,
    image_digest: &str,
    layer_index: usize,
) -> anyhow::Result<Vec<u8>> {
    use smolvm_protocol::AgentRequest;

    const LAYER_EXPORT_TIMEOUT: Duration = Duration::from_secs(600);

    let request = AgentRequest::ExportLayer {
        image_digest: image_digest.to_string(),
        layer_index,
    };

    let _timeout_guard = client.set_extended_read_timeout(LAYER_EXPORT_TIMEOUT)?;
    client.send_raw(&request)?;

    let start = Instant::now();
    let mut result = Vec::new();
    loop {
        if start.elapsed() > LAYER_EXPORT_TIMEOUT {
            anyhow::bail!(
                "layer export timed out after {}s ({} bytes received)",
                LAYER_EXPORT_TIMEOUT.as_secs(),
                result.len()
            );
        }

        let response = client.recv_raw()?;
        match response {
            AgentResponse::DataChunk { data, done } => {
                result.extend_from_slice(&data);
                if done {
                    return Ok(result);
                }
            }
            AgentResponse::Error { message, .. } => {
                anyhow::bail!("agent error: {}", message);
            }
            _ => {
                anyhow::bail!("unexpected response type during layer export");
            }
        }
    }
}
