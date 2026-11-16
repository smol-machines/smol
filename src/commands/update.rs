//! smol update — modify settings on a stopped machine.
//!
//! Faithful port of the engine's `machine update` over the public smolvm lib:
//! edits the DB record (mounts, ports, resources, env, net, gpu, workdir) and
//! expands disk files (expand-only) in a single consistent update.

use clap::Args;
use smolvm::cli::vm_common::expand_disks;
use smolvm::config::RecordState;
use smolvm::data::network::PortMapping;
use smolvm::data::storage::HostMount;
use smolvm::db::SmolvmDb;

#[derive(Args, Debug)]
pub struct UpdateCmd {
    /// Accepted so scripts can pass `--local` to every machine command; this
    /// command only manages local machines.
    #[arg(long = "local")]
    pub _local: bool,

    /// Machine to update
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: String,

    /// Add a host mount. `:staged` uses a guest-local working copy until sync or stop.
    #[arg(short = 'v', long = "volume", value_name = "HOST:GUEST[:ro|rw|staged]")]
    pub volume: Vec<String>,
    /// Remove volume mount (HOST:GUEST)
    #[arg(long = "remove-volume", value_name = "HOST:GUEST")]
    pub remove_volume: Vec<String>,

    /// Add port mapping (HOST:GUEST)
    #[arg(short = 'p', long = "port", value_parser = PortMapping::parse, value_name = "HOST:GUEST[/udp]")]
    pub port: Vec<PortMapping>,
    /// Remove port mapping (HOST:GUEST)
    #[arg(long = "remove-port", value_parser = PortMapping::parse, value_name = "HOST:GUEST[/udp]")]
    pub remove_port: Vec<PortMapping>,

    /// Set vCPU count
    #[arg(long, value_name = "N")]
    pub cpus: Option<u8>,
    /// Set memory in MiB
    #[arg(long, value_name = "MiB")]
    pub mem: Option<u32>,

    /// Enable outbound network access
    #[arg(long)]
    pub net: bool,
    /// Disable outbound network access
    #[arg(long)]
    pub no_net: bool,

    /// Remove the required external interceptor binding from a stopped machine.
    #[arg(long)]
    pub no_egress_interceptor: bool,

    /// Add/replace environment variable (KEY=VALUE)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Remove environment variable by key
    #[arg(long = "remove-env", value_name = "KEY")]
    pub remove_env: Vec<String>,

    /// Set working directory
    #[arg(short = 'w', long, value_name = "DIR")]
    pub workdir: Option<String>,

    /// Enable GPU acceleration
    #[arg(long)]
    pub gpu: bool,
    /// Disable GPU acceleration
    #[arg(long)]
    pub no_gpu: bool,

    /// Storage disk size in GiB (expand only)
    #[arg(long, value_name = "GiB")]
    pub storage: Option<u64>,
    /// Overlay disk size in GiB (expand only)
    #[arg(long, value_name = "GiB")]
    pub overlay: Option<u64>,
}

impl UpdateCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let db = SmolvmDb::open()?;
        let record = db
            .get_vm(&self.name)?
            .ok_or_else(|| anyhow::anyhow!("machine '{}' not found", self.name))?;

        // Must be stopped.
        match record.actual_state() {
            RecordState::Stopped | RecordState::Created => {}
            other => anyhow::bail!("machine must be stopped to update (is {other:?})"),
        }

        // Validate proposed resources via the same path machine start uses.
        let proposed = smolvm::agent::VmResources {
            cpus: self.cpus.unwrap_or(record.cpus),
            memory_mib: self.mem.unwrap_or(record.mem),
            ..record.vm_resources()
        };
        proposed
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid resources: {e}"))?;

        for spec in &self.env {
            match spec.split_once('=') {
                Some((key, _)) if !key.is_empty() => {}
                _ => anyhow::bail!("invalid env format '{spec}': expected KEY=VALUE"),
            }
        }

        // Remote volumes are mounted inside the guest by the agent, so peel
        // them off before the host-directory parse, which would reject an
        // `s3://` source as a missing directory.
        let (host_volume_specs, new_remote_volumes) =
            smolvm::remote_volume::split_specs(&self.volume)?;
        let new_mounts = HostMount::parse(&host_volume_specs)?;

        // Preserve mount modes and ordering while applying additions/removals.
        let mut final_mounts = record.host_mounts();
        for rm in &self.remove_volume {
            let rm_without_mode = rm
                .strip_suffix(":ro")
                .or_else(|| rm.strip_suffix(":rw"))
                .or_else(|| rm.strip_suffix(":staged"))
                .unwrap_or(rm);
            let canonical_rm = if let Some((rm_src, rm_tgt)) = rm_without_mode.rsplit_once(':') {
                let resolved = std::fs::canonicalize(rm_src)
                    .unwrap_or_else(|_| std::path::PathBuf::from(rm_src));
                Some((resolved, std::path::PathBuf::from(rm_tgt)))
            } else {
                None
            };
            final_mounts.retain(|mount| {
                canonical_rm.as_ref().is_none_or(|(source, target)| {
                    mount.source != *source || mount.target != *target
                })
            });
        }
        for mount in &new_mounts {
            if !final_mounts
                .iter()
                .any(|current| current.source == mount.source && current.target == mount.target)
            {
                final_mounts.push(mount.clone());
            }
        }
        HostMount::ensure_unique_targets(&final_mounts)?;
        let (final_live_mounts, final_staged_mounts) =
            HostMount::split_storage_tuples(&final_mounts);

        // Reject duplicate host ports after the proposed changes.
        {
            let mut final_ports: Vec<PortMapping> = record
                .port_mappings()
                .into_iter()
                .filter(|existing| !self.remove_port.contains(existing))
                .collect();
            for p in &self.port {
                if !final_ports.contains(p) {
                    final_ports.push(*p);
                }
            }
            PortMapping::check_duplicates(&final_ports).map_err(|e| anyhow::anyhow!("{e}"))?;
        }

        // Expand physical disks before the DB write so a failure leaves the
        // record untouched.
        let mut changes: Vec<String> = Vec::new();
        if self.storage.is_some() || self.overlay.is_some() {
            changes.extend(expand_disks(
                &self.name,
                &record,
                self.storage,
                self.overlay,
            )?);
        }

        db.update_vm(&self.name, |r| {
            if self.no_egress_interceptor && r.external_interceptor_required {
                r.external_interceptor_required = false;
                changes.push("  external egress interceptor: disabled".into());
            }
            if let Some(s) = self.storage {
                r.storage_gb = Some(s);
            }
            if let Some(o) = self.overlay {
                r.overlay_gb = Some(o);
            }
            if !self.remove_volume.is_empty() || !new_mounts.is_empty() {
                for rm in &self.remove_volume {
                    changes.push(format!("  removed volume: {rm}"));
                }
                for mount in &new_mounts {
                    let mode = if mount.staged {
                        ":staged"
                    } else if mount.read_only {
                        ":ro"
                    } else {
                        ""
                    };
                    changes.push(format!(
                        "  added volume: {}:{}{}",
                        mount.source.display(),
                        mount.target.display(),
                        mode
                    ));
                }
                r.mounts = final_live_mounts.clone();
                r.staged_mounts = final_staged_mounts.clone();
            }
            for rv in &new_remote_volumes {
                if !r
                    .remote_volumes
                    .iter()
                    .any(|e| e.source == rv.source && e.target == rv.target)
                {
                    changes.push(format!(
                        "  added remote volume: {}:{}{}",
                        rv.source,
                        rv.target,
                        if rv.read_only { ":ro" } else { "" }
                    ));
                    r.remote_volumes.push(rv.clone());
                }
            }
            let mut ports = r.port_mappings();
            for rm in &self.remove_port {
                let before = ports.len();
                ports.retain(|existing| existing != rm);
                if ports.len() < before {
                    changes.push(format!("  removed port: {}", rm.label()));
                }
            }
            for p in &self.port {
                if !ports.contains(p) {
                    changes.push(format!("  added port: {}", p.label()));
                    ports.push(*p);
                }
            }
            r.set_port_mappings(&ports);
            if let Some(cpus) = self.cpus {
                changes.push(format!("  cpus: {} → {cpus}", r.cpus));
                r.cpus = cpus;
            }
            if let Some(mem) = self.mem {
                changes.push(format!("  memory: {} MiB → {mem} MiB", r.mem));
                r.mem = mem;
            }
            if self.net {
                changes.push("  network: enabled".into());
                r.network = true;
            }
            if self.no_net {
                changes.push("  network: disabled".into());
                r.network = false;
                if r.allowed_cidrs.is_some() {
                    changes.push("  cleared allow_cidrs".into());
                    r.allowed_cidrs = None;
                }
                if r.dns_filter_hosts.is_some() {
                    changes.push("  cleared dns_filter_hosts".into());
                    r.dns_filter_hosts = None;
                }
            }
            for rm_key in &self.remove_env {
                let before = r.env.len();
                r.env.retain(|(k, _)| k != rm_key);
                if r.env.len() < before {
                    changes.push(format!("  removed env: {rm_key}"));
                }
            }
            for spec in &self.env {
                if let Some((key, val)) = spec.split_once('=') {
                    r.env.retain(|(k, _)| k != key);
                    r.env.push((key.to_string(), val.to_string()));
                    changes.push(format!("  env: {key}={val}"));
                }
            }
            if let Some(ref wd) = self.workdir {
                changes.push(format!("  workdir: {wd}"));
                r.workdir = Some(wd.clone());
            }
            if self.gpu {
                changes.push("  gpu: enabled".into());
                r.gpu = Some(true);
            }
            if self.no_gpu {
                changes.push("  gpu: disabled".into());
                r.gpu = Some(false);
            }
        })?;

        if changes.is_empty() {
            println!("No changes specified.");
        } else {
            println!("Updated machine '{}':", self.name);
            for change in &changes {
                println!("{change}");
            }
        }
        Ok(())
    }
}
