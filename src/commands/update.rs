//! smol update — modify settings on a stopped machine.
//!
//! Faithful port of the engine's `machine update` over the public smolvm lib:
//! edits the DB record (mounts, ports, resources, env, net, gpu, workdir) and
//! expands disk files (expand-only) in a single consistent update.

use super::common::format_bytes;
use clap::Args;
use smolvm::config::RecordState;
use smolvm::data::network::PortMapping;
use smolvm::data::storage::HostMount;
use smolvm::db::SmolvmDb;

#[derive(Args, Debug)]
pub struct UpdateCmd {
    /// Machine to update
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: String,

    /// Add volume mount (HOST:GUEST[:ro])
    #[arg(short = 'v', long = "volume", value_name = "HOST:GUEST[:ro]")]
    pub volume: Vec<String>,
    /// Remove volume mount (HOST:GUEST)
    #[arg(long = "remove-volume", value_name = "HOST:GUEST")]
    pub remove_volume: Vec<String>,

    /// Add port mapping (HOST:GUEST)
    #[arg(short = 'p', long = "port", value_parser = PortMapping::parse, value_name = "HOST:GUEST")]
    pub port: Vec<PortMapping>,
    /// Remove port mapping (HOST:GUEST)
    #[arg(long = "remove-port", value_parser = PortMapping::parse, value_name = "HOST:GUEST")]
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

        let new_mounts = HostMount::parse(&self.volume)?;

        // Reject duplicate host ports after the proposed changes.
        {
            let mut final_ports: Vec<PortMapping> = record
                .ports
                .iter()
                .filter(|&&(h, g)| {
                    !self.remove_port.iter().any(|rm| rm.host == h && rm.guest == g)
                })
                .map(|&(h, g)| PortMapping::new(h, g))
                .collect();
            for p in &self.port {
                if !final_ports
                    .iter()
                    .any(|e| e.host == p.host && e.guest == p.guest)
                {
                    final_ports.push(*p);
                }
            }
            PortMapping::check_duplicates(&final_ports)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }

        // Expand physical disks before the DB write so a failure leaves the
        // record untouched.
        let mut changes: Vec<String> = Vec::new();
        if self.storage.is_some() || self.overlay.is_some() {
            changes.extend(expand_disks(&self.name, &record, self.storage, self.overlay)?);
        }

        db.update_vm(&self.name, |r| {
            if let Some(s) = self.storage {
                r.storage_gb = Some(s);
            }
            if let Some(o) = self.overlay {
                r.overlay_gb = Some(o);
            }
            for rm in &self.remove_volume {
                let canonical_rm = if let Some((rm_src, rm_tgt)) = rm.split_once(':') {
                    let resolved = std::fs::canonicalize(rm_src)
                        .unwrap_or_else(|_| std::path::PathBuf::from(rm_src));
                    format!("{}:{}", resolved.display(), rm_tgt)
                } else {
                    rm.clone()
                };
                let before = r.mounts.len();
                r.mounts.retain(|(src, tgt, _)| {
                    let spec = format!("{src}:{tgt}");
                    spec != canonical_rm && spec != *rm
                });
                if r.mounts.len() < before {
                    changes.push(format!("  removed volume: {rm}"));
                }
            }
            for m in &new_mounts {
                let tuple = m.to_storage_tuple();
                if !r.mounts.iter().any(|(s, t, _)| *s == tuple.0 && *t == tuple.1) {
                    changes.push(format!(
                        "  added volume: {}:{}{}",
                        tuple.0,
                        tuple.1,
                        if tuple.2 { ":ro" } else { "" }
                    ));
                    r.mounts.push(tuple);
                }
            }
            for rm in &self.remove_port {
                let before = r.ports.len();
                r.ports.retain(|&(h, g)| h != rm.host || g != rm.guest);
                if r.ports.len() < before {
                    changes.push(format!("  removed port: {}:{}", rm.host, rm.guest));
                }
            }
            for p in &self.port {
                let tuple = p.to_tuple();
                if !r.ports.contains(&tuple) {
                    changes.push(format!("  added port: {}:{}", tuple.0, tuple.1));
                    r.ports.push(tuple);
                }
            }
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

/// Expand a machine's storage/overlay disk files (expand-only). Port of the
/// engine's vm_common::expand_disks over `smolvm::storage::expand_disk`.
fn expand_disks(
    name: &str,
    record: &smolvm::config::VmRecord,
    new_storage_gb: Option<u64>,
    new_overlay_gb: Option<u64>,
) -> anyhow::Result<Vec<String>> {
    use smolvm::data::disk::{Overlay, Storage};
    use smolvm::storage::{expand_disk, DEFAULT_OVERLAY_SIZE_GIB, DEFAULT_STORAGE_SIZE_GIB};

    let cur_storage = record.storage_gb.unwrap_or(DEFAULT_STORAGE_SIZE_GIB);
    let cur_overlay = record.overlay_gb.unwrap_or(DEFAULT_OVERLAY_SIZE_GIB);

    if let Some(s) = new_storage_gb {
        if s < cur_storage {
            anyhow::bail!("storage disk cannot be shrunk from {cur_storage} GiB to {s} GiB (expand only)");
        }
    }
    if let Some(o) = new_overlay_gb {
        if o < cur_overlay {
            anyhow::bail!("overlay disk cannot be shrunk from {cur_overlay} GiB to {o} GiB (expand only)");
        }
    }

    let manager = smolvm::agent::AgentManager::for_vm(name)
        .map_err(|e| anyhow::anyhow!("get agent manager: {e}"))?;
    let mut changes = Vec::new();

    if let Some(storage_gb) = new_storage_gb {
        if storage_gb > cur_storage {
            expand_disk::<Storage>(manager.storage_path(), storage_gb)
                .map_err(|e| anyhow::anyhow!("expand storage disk: {e}"))?;
            changes.push(format!(
                "  storage: {cur_storage} GiB → {storage_gb} GiB ({})",
                format_bytes(storage_gb * 1024 * 1024 * 1024)
            ));
        }
    }
    if let Some(overlay_gb) = new_overlay_gb {
        if overlay_gb > cur_overlay {
            expand_disk::<Overlay>(manager.overlay_path(), overlay_gb)
                .map_err(|e| anyhow::anyhow!("expand overlay disk: {e}"))?;
            changes.push(format!(
                "  overlay: {cur_overlay} GiB → {overlay_gb} GiB ({})",
                format_bytes(overlay_gb * 1024 * 1024 * 1024)
            ));
        }
    }
    Ok(changes)
}
