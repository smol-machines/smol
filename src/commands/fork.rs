//! smol machine fork — clone a running, forkable machine (copy-on-write RAM + disks).
//!
//! Mirrors the engine's `machine fork` (src/cli/vm_common.rs::fork_vm) over the
//! public `smolvm` library, matching smol's existing pattern of reimplementing
//! engine flows rather than calling binary-internal helpers. The golden must
//! have been started forkable (`smol machine start --forkable`), which leaves a control
//! socket the engine uses to freeze it and write a snapshot.

use clap::Args;
use smolvm::agent::{resolve_disk_image, vm_data_dir, AgentClient, AgentManager, LaunchFeatures};
use smolvm::config::RecordState;
use smolvm::data::network::PortMapping;
use smolvm::db::SmolvmDb;
use smolvm::platform::uds::UdsStream;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct ForkCmd {
    /// The running, forkable source machine to clone from
    #[arg(long, value_name = "NAME")]
    pub golden: String,

    /// Name for the new clone machine
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: String,

    /// (Rejected) make the clone itself forkable — nested fork is unsupported
    #[arg(long)]
    pub forkable: bool,

    /// Pin the clone's inbound port forwards (repeatable). Without this, the
    /// golden's forwards are remapped to freshly-allocated host ports.
    #[arg(short = 'p', long = "port", value_parser = PortMapping::parse, value_name = "HOST:GUEST")]
    pub port: Vec<PortMapping>,

    /// Fork on the cloud control plane (live-RAM CoW clone on the golden's node)
    /// instead of locally. The golden must have been started `--forkable --cloud`.
    /// Usually unnecessary — the golden's location is resolved automatically.
    #[arg(long)]
    pub cloud: bool,

    /// Force a local fork. Equivalent to a `local/` prefix on the golden.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,

    /// Share the golden's loaded CUDA weights with this clone instead of
    /// copying them — sibling clones then keep ONE copy of the base model in
    /// VRAM. Correct when the base stays frozen (LoRA/QLoRA fine-tuning,
    /// inference); use a plain fork when the clone trains the base weights.
    #[arg(long)]
    pub share_weights: bool,
}

impl ForkCmd {
    pub fn run(mut self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        // The clone lands wherever its golden lives: resolve the golden's
        // location (+ optional --local/--cloud), then route.
        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, golden_handle) = resolve::route(Some(&self.golden), target)?;
        if location == Location::Cloud {
            return self.run_cloud();
        }
        // Use the prefix-stripped handle as the local golden reference.
        self.golden = golden_handle;
        let golden = &self.golden;
        let clone = &self.name;

        smolvm::data::validate_vm_name(clone, "clone name")
            .map_err(|e| anyhow::anyhow!("clone name: {e}"))?;

        // A clone boots from a copy-on-write MAP_PRIVATE mapping of the golden's
        // RAM, not a fresh memfd, so it cannot itself be re-forked.
        if self.forkable {
            anyhow::bail!(
                "nested fork is not supported: a clone cannot be re-forked, so \
                 `--forkable` on a fork has no effect (drop it)"
            );
        }

        let db = SmolvmDb::open()?;
        let golden_rec = db
            .get_vm(golden)?
            .ok_or_else(|| anyhow::anyhow!("machine '{golden}' not found"))?;

        // The golden must be alive and forkable. Probe the control socket (after
        // its first fork the golden is frozen, so an agent ping would fail, but
        // STATUS still answers).
        let ctl = vm_data_dir(golden).join("control.sock");
        if !ctl.exists() {
            anyhow::bail!(
                "golden '{golden}' is not running forkable; start it with \
                 `smol machine start --forkable --name {golden}`"
            );
        }
        let status = control_socket_cmd(&ctl, "STATUS").map_err(|e| {
            anyhow::anyhow!(
                "golden '{golden}' control socket not responding ({e}); start it with \
                 `smol machine start --forkable --name {golden}`"
            )
        })?;
        if !status.starts_with("OK") {
            anyhow::bail!("golden '{golden}' is not ready to fork: {status}");
        }
        if db.get_vm(clone)?.is_some() {
            anyhow::bail!("machine '{clone}' already exists");
        }

        // Clone dir must be PRISTINE at disk-clone time: a leftover directory
        // (orphan of a crashed fork) holds stale qcow2 overlays that make
        // krun_create_disk_overlay refuse with rc=-5. The DB check above
        // guarantees no live clone owns this name, so clearing is safe.
        let clone_dir = vm_data_dir(clone);
        if clone_dir.exists() {
            std::fs::remove_dir_all(&clone_dir)?;
        }
        std::fs::create_dir_all(&clone_dir)?;
        // The snapshot lives under the GOLDEN's dir (gdir/fork-snapshots/<clone>),
        // matching the engine: the frozen golden VMM writes the checkpoint AND
        // pre-creates disk overlays NEXT TO the snapshot dir — putting it inside
        // the clone dir made those collide with clone_disks below (rc=-5), and a
        // Landlock-confined golden could not write outside its own dir anyway.
        let gdir = vm_data_dir(golden);
        let snapshot_dir = gdir.join("fork-snapshots").join(clone);
        std::fs::create_dir_all(&snapshot_dir)?;

        // Clone the golden's config, clear running-state, and remap inbound
        // ports to fresh host ports (TSI gives each clone outbound for free;
        // only inbound must be made distinct so clones don't collide).
        let mut clone_rec = golden_rec.clone();
        clone_rec.name = clone.clone();
        clone_rec.pid = None;
        clone_rec.pid_start_time = None;
        let pinned = PortMapping::to_tuples(&self.port);
        if !pinned.is_empty() {
            clone_rec.ports = pinned;
            for (h, g) in &clone_rec.ports {
                eprintln!("  port {h}->{g} (pinned)");
            }
        } else if !clone_rec.ports.is_empty() {
            let mut remapped = Vec::with_capacity(clone_rec.ports.len());
            for (golden_host, guest) in &clone_rec.ports {
                match alloc_free_host_port() {
                    Some(h) => {
                        eprintln!(
                            "  port {golden_host}->{guest} (golden) remapped to {h}->{guest} (clone)"
                        );
                        remapped.push((h, *guest));
                    }
                    None => eprintln!(
                        "  warning: could not allocate a host port for guest port {guest}; dropping it"
                    ),
                }
            }
            clone_rec.ports = remapped;
        }
        clone_rec.golden = Some(golden.clone());
        db.insert_vm(clone, &clone_rec)?;

        // Freeze the golden and write its snapshot (checkpoint + memfd manifest).
        eprintln!("Freezing golden '{golden}' as fork base...");
        let reply = match control_socket_cmd(&ctl, &format!("FORK {}", snapshot_dir.display())) {
            Ok(r) => r,
            Err(e) => {
                rollback(&db, clone, &clone_dir);
                return Err(e);
            }
        };
        if !reply.starts_with("OK") {
            rollback(&db, clone, &clone_dir);
            anyhow::bail!("golden FORK failed: {reply}");
        }

        // Give the clone its own copy-on-write disks over the (now frozen) golden.
        if let Err(e) = clone_disks(&gdir, &clone_dir) {
            rollback(&db, clone, &clone_dir);
            return Err(e);
        }

        // Boot the clone from the snapshot (env-driven restore) instead of cold
        // booting — no init/image steps; it restores the golden's live state.
        let mounts = clone_rec.host_mounts();
        let ports = clone_rec.port_mappings();
        let resources = clone_rec.vm_resources();
        let features = LaunchFeatures {
            dns_filter_hosts: clone_rec.dns_filter_hosts.clone(),
            // Boot the clone from the golden's snapshot (restore live RAM) — MUST
            // go on the features: the manager forwards SMOLVM_SNAPSHOT_DIR to the
            // boot subprocess from `features`, not from a (non-inherited) process
            // env. Without this the clone cold-boots and loses the warm RAM.
            snapshot_dir: Some(snapshot_dir.clone()),
            cuda_share_weights: self.share_weights,
            // `smol machine fork` detaches the clone to persist; opt out of the boot
            // subprocess's parent-death watchdog (see start.rs) so it survives
            // this command's exit.
            watch_parent: Some(false),
            ..Default::default()
        };
        let manager = match AgentManager::for_vm_with_sizes(
            clone,
            clone_rec.storage_gb,
            clone_rec.overlay_gb,
        ) {
            Ok(m) => m,
            Err(e) => {
                rollback(&db, clone, &clone_dir);
                return Err(anyhow::anyhow!("create agent manager: {e}"));
            }
        };
        eprintln!("Booting clone '{clone}' from snapshot...");
        let launched = manager.ensure_running_with_full_config(mounts, ports, resources, features);
        if let Err(e) = launched {
            rollback(&db, clone, &clone_dir);
            return Err(anyhow::anyhow!("boot clone: {e}"));
        }

        let pid = manager.child_pid();
        let pid_start_time = pid.and_then(smolvm::process::process_start_time);
        if let Err(e) = db.update_vm(clone, |r| {
            r.state = RecordState::Running;
            r.pid = pid;
            r.pid_start_time = pid_start_time;
        }) {
            eprintln!("Warning: failed to persist clone state: {e}");
        }

        // A clone inherits the golden's hostname, machine-id and RNG state;
        // rejuvenate so the streams diverge (best-effort).
        rejuvenate_clone(clone, manager.vsock_socket());

        manager.detach();
        eprintln!(
            "Forked '{golden}' -> '{clone}' (PID {}). Golden stays frozen as the fork base \
             (do not start it again while clones exist).",
            pid.unwrap_or(0)
        );
        Ok(())
    }

    /// Cloud fork: POST `/v1/machines/{golden}/fork` to the control plane, which
    /// pins the live-RAM CoW clone to the golden's node. `--golden` resolves to
    /// the source machine id; the clone name + pinned ports go in the body.
    fn run_cloud(self) -> anyhow::Result<()> {
        if self.forkable {
            anyhow::bail!(
                "nested fork is not supported: a clone cannot be re-forked, so \
                 `--forkable` on a fork has no effect (drop it)"
            );
        }
        let clone_name = self.name.clone();
        // The control plane's MachinePort is { port: <guest>, hostPort: <host?> }.
        let ports: Vec<serde_json::Value> = self
            .port
            .iter()
            .map(|p| serde_json::json!({ "port": p.guest, "hostPort": p.host }))
            .collect();
        super::cloud::run_cloud_command(
            Some(self.golden),
            move |http, endpoint, golden_id| async move {
                eprintln!("Forking {golden_id} -> {clone_name}...");
                let resp = http
                    .post(format!("{}/v1/machines/{}/fork", endpoint, golden_id))
                    .json(&serde_json::json!({ "name": clone_name, "ports": ports }))
                    .send()
                    .await?;
                match resp.status().as_u16() {
                    200 | 201 => {
                        let machine: super::cloud::CloudMachine = resp.json().await?;
                        println!(
                            "Machine '{}' ({}): {}",
                            machine.name.as_deref().unwrap_or(&clone_name),
                            machine.id,
                            machine.state
                        );
                    }
                    404 => anyhow::bail!("golden '{}' not found", golden_id),
                    409 => {
                        let text = resp.text().await.unwrap_or_default();
                        anyhow::bail!("cannot fork golden '{}': {}", golden_id, text);
                    }
                    _ => {
                        super::cloud::check_response(resp, "fork golden").await?;
                    }
                }
                Ok(())
            },
        )
    }
}

/// Remove a partially-created clone after a failure mid-fork.
fn rollback(db: &SmolvmDb, clone: &str, clone_dir: &Path) {
    let _ = db.remove_vm(clone);
    let _ = std::fs::remove_dir_all(clone_dir);
}

/// Give the clone copy-on-write disks backed by the (frozen) golden's. Linux
/// uses qcow2 overlays (O(metadata)); macOS clonefiles (APFS CoW). The
/// `.formatted` marker is copied so the clone never reformats the inherited fs.
fn clone_disks(gdir: &Path, clone_dir: &Path) -> anyhow::Result<()> {
    use smolvm::data::disk::DiskFormat;
    use smolvm::data::storage::{OVERLAY_DISK_FILENAME, STORAGE_DISK_FILENAME};

    let disks: Vec<(&str, std::path::PathBuf, DiskFormat)> =
        [STORAGE_DISK_FILENAME, OVERLAY_DISK_FILENAME]
            .into_iter()
            .map(|raw| {
                let (src, fmt) = resolve_disk_image(gdir, raw);
                (raw, src, fmt)
            })
            .filter(|(_, src, _)| src.exists())
            .collect();

    #[cfg(target_os = "linux")]
    {
        let mut specs = Vec::with_capacity(disks.len());
        for (raw, src, fmt) in &disks {
            let base = src
                .canonicalize()
                .map_err(|e| anyhow::anyhow!("clone disk {}: {e}", src.display()))?;
            let overlay = clone_dir.join(Path::new(raw).with_extension("qcow2"));
            specs.push((overlay, base, *fmt));
        }
        if std::env::var_os("SMOL_FORK_DEBUG").is_some() {
            for (overlay, base, fmt) in &specs {
                eprintln!(
                    "[fork-dbg] overlay={} exists={} | base={} exists={} fmt={:?}",
                    overlay.display(),
                    overlay.exists(),
                    base.display(),
                    base.exists(),
                    fmt
                );
            }
            if let Ok(rd) = std::fs::read_dir(clone_dir) {
                for e in rd.flatten() {
                    eprintln!(
                        "[fork-dbg] clone_dir has: {}",
                        e.file_name().to_string_lossy()
                    );
                }
            }
        }
        smolvm::agent::create_disk_overlays(&specs)
            .map_err(|e| anyhow::anyhow!("create clone overlays: {e}"))?;
        for (raw, _, _) in &disks {
            let marker = Path::new(raw).with_extension("formatted");
            let src_marker = gdir.join(&marker);
            if src_marker.exists() {
                let _ = std::fs::copy(&src_marker, clone_dir.join(&marker));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        for (_, src, _) in &disks {
            let dst = clone_dir.join(src.file_name().unwrap());
            smolvm::disk_utils::clone_or_copy_file(src, &dst)
                .map_err(|e| anyhow::anyhow!("clone disk {}: {e}", src.display()))?;
            let src_marker = src.with_extension("formatted");
            if src_marker.exists() {
                let _ = std::fs::copy(&src_marker, dst.with_extension("formatted"));
            }
        }
    }
    Ok(())
}

/// Best-effort identity rejuvenation: unique hostname, fresh machine-id, and a
/// stir of the kernel RNG with host entropy so clones don't share streams.
fn rejuvenate_clone(clone: &str, sock: &Path) {
    let seed = host_random_hex(64);
    // `clone` is validated (alphanumeric + dashes), so single-quoting is safe.
    let script = format!(
        "hostname '{c}' 2>/dev/null; printf '%s\\n' '{c}' > /etc/hostname 2>/dev/null; \
         (cat /proc/sys/kernel/random/uuid 2>/dev/null | tr -d '-' > /etc/machine-id) 2>/dev/null; \
         printf '%s' '{s}' > /dev/urandom 2>/dev/null; true",
        c = clone,
        s = seed,
    );
    let mut client = match AgentClient::connect_with_retry(sock) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Warning: clone '{clone}' rejuvenation skipped (agent connect: {e})");
            return;
        }
    };
    match client.vm_exec(
        vec!["/bin/sh".into(), "-c".into(), script],
        vec![],
        None,
        Some(Duration::from_secs(10)),
        None,
    ) {
        Ok((0, _, _)) => {}
        Ok((code, _, stderr)) => eprintln!(
            "Warning: clone '{clone}' rejuvenation exited {code}: {}",
            String::from_utf8_lossy(&stderr).trim()
        ),
        Err(e) => eprintln!("Warning: clone '{clone}' rejuvenation failed: {e}"),
    }
}

/// Allocate a free host TCP port by binding to :0 and reading it back.
fn alloc_free_host_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|addr| addr.port())
}

/// `hex_len/2` random bytes from the host RNG, hex-encoded.
fn host_random_hex(hex_len: usize) -> String {
    let mut buf = vec![0u8; hex_len / 2];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Send one line to a VM control socket and return its reply line.
fn control_socket_cmd(sock: &Path, cmd: &str) -> anyhow::Result<String> {
    let mut stream =
        UdsStream::connect(sock).map_err(|e| anyhow::anyhow!("connect control socket: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();
    stream
        .write_all(format!("{cmd}\n").as_bytes())
        .map_err(|e| anyhow::anyhow!("write control socket: {e}"))?;
    let mut reply = String::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                reply.push(byte[0] as char);
            }
            Err(e) => return Err(anyhow::anyhow!("read control socket: {e}")),
        }
    }
    Ok(reply)
}
