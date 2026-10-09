//! NapiMachine — the main NAPI class for embedded Machine operations.
//!
//! All blocking operations run on tokio's blocking thread pool. VM process
//! handles live in a process-local runtime registry so multiple JS objects and
//! worker threads coordinate through the same cached handle per machine name.

use napi::bindgen_prelude::Buffer;
use napi_derive::napi;

use crate::error::IntoNapiResult;
use crate::types::*;
use smolvm::agent::ExecEvent;
use smolvm::embedded::{runtime, ForkSourcePolicy, MachineSpec};

fn join_error(err: tokio::task::JoinError) -> napi::Error {
    napi::Error::from_reason(format!("Task join error: {}", err))
}

/// Check every step's target before any of them is applied.
fn check_resize_targets(name: &str, steps: &[smolvm::embedded::ResizeSpec]) -> smolvm::Result<()> {
    smolvm::agent::live_resize::check_targets(
        &smolvm::db::SmolvmDb::open()?,
        name,
        steps.iter().find_map(|s| s.cpus),
        steps.iter().find_map(|s| s.memory_mib),
        steps.iter().find_map(|s| s.storage_gib),
        steps.iter().find_map(|s| s.overlay_gib),
    )
}

/// Split a resize into the one-resource requests the engine applies.
fn resize_steps(spec: &ResizeConfig) -> napi::Result<Vec<smolvm::embedded::ResizeSpec>> {
    let gib = |value: Option<f64>, field: &str| -> napi::Result<Option<u64>> {
        match value {
            Some(g) if g.fract() != 0.0 || g < 1.0 || g > u64::MAX as f64 => Err(
                napi::Error::from_reason(format!("{field} must be a positive whole number")),
            ),
            Some(g) => Ok(Some(g as u64)),
            None => Ok(None),
        }
    };
    let storage_gib = gib(spec.storage_gib, "storageGb")?;
    let overlay_gib = gib(spec.overlay_gib, "overlayGb")?;
    // RAM first: its host headroom check is the likeliest refusal, and it
    // refuses before anything changes.
    let mut steps = Vec::new();
    if let Some(memory_mib) = spec.memory_mib {
        steps.push(smolvm::embedded::ResizeSpec {
            memory_mib: Some(memory_mib),
            ..Default::default()
        });
    }
    if let Some(cpus) = spec.cpus {
        steps.push(smolvm::embedded::ResizeSpec {
            cpus: Some(cpus),
            ..Default::default()
        });
    }
    if storage_gib.is_some() || overlay_gib.is_some() {
        steps.push(smolvm::embedded::ResizeSpec {
            storage_gib,
            overlay_gib,
            ..Default::default()
        });
    }
    if steps.is_empty() {
        return Err(napi::Error::from_reason(
            "resize needs at least one of cpus, memoryMb, storageGb or overlayGb",
        ));
    }
    Ok(steps)
}

fn parse_interceptor(
    address: Option<String>,
    token: Option<String>,
) -> napi::Result<Option<smolvm_protocol::InterceptEndpoint>> {
    match (address, token) {
        (None, None) => Ok(None),
        (Some(address), Some(token)) => smolvm::embedded::interceptor_endpoint(&address, &token)
            .map(Some)
            .into_napi(),
        _ => Err(napi::Error::from_reason(
            "egress interceptor requires both address and token",
        )),
    }
}

fn source_policy(freeze_source: Option<bool>) -> ForkSourcePolicy {
    if freeze_source.unwrap_or(false) {
        ForkSourcePolicy::Freeze
    } else {
        ForkSourcePolicy::PlatformDefault
    }
}

#[napi]
pub struct NapiMachine {
    name: String,
}

/// A live, incremental exec stream. `next()` resolves with the next event as it
/// arrives (off the libuv loop via spawn_blocking), or `null` once the command
/// exits and the channel closes. Mirrors the Python SDK's ExecStream iterator.
#[napi]
pub struct ExecStream {
    rx: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<ExecEvent>>>,
    cancel: smolvm::embedded::ExecCancel,
}

#[napi]
impl ExecStream {
    #[napi]
    pub async fn next(&self) -> napi::Result<Option<ExecStreamEvent>> {
        let rx = self.rx.clone();
        let received =
            tokio::task::spawn_blocking(move || rx.lock().expect("exec stream lock").recv())
                .await
                .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        Ok(received.ok().map(ExecStreamEvent::from))
    }

    /// Kill the command: its connection closes and the guest agent kills it.
    /// Pending `next()` calls then resolve `null`. Idempotent, and a no-op
    /// once the command has exited.
    #[napi]
    pub fn kill(&self) {
        self.cancel.cancel();
    }
}

#[napi]
impl NapiMachine {
    /// Probe whether this host can run local machines, without booting one.
    ///
    /// Runs the same checks a local `create()` fails on — `/dev/kvm` access on
    /// Linux, and a locatable libkrun on every platform — and reports the same
    /// error code, so a caller can pick a sandbox up front instead of paying
    /// for a failed boot. Never throws.
    #[napi]
    pub fn check_host() -> HostAvailability {
        let unavailable = |err: smolvm::Error| {
            let (code, reason) = crate::error::code_and_message(&err);
            HostAvailability {
                available: false,
                code: Some(code.to_string()),
                reason: Some(reason),
            }
        };
        #[cfg(target_os = "linux")]
        if let Err(err) = smolvm::platform::linux::check_kvm_available() {
            return unavailable(err);
        }
        match smolvm::vm::backend::LibkrunBackend::new() {
            Ok(backend) if smolvm::vm::VmBackend::is_available(&backend) => HostAvailability {
                available: true,
                code: None,
                reason: None,
            },
            Ok(_) => unavailable(smolvm::Error::HypervisorUnavailable(
                "libkrun was not found; reinstall the SDK or set SMOLVM_LIB_DIR".into(),
            )),
            Err(err) => unavailable(err),
        }
    }

    /// Create a new machine. Does not start the VM yet — call `start()`.
    #[napi(constructor)]
    pub fn new(config: MachineConfig) -> napi::Result<Self> {
        // An `s3://` source is mounted inside the guest by the agent, not
        // bind-mounted from the host, so route it to the remote-volume list
        // instead of the host-directory parse (which would reject it as a
        // missing directory).
        let (remote_specs, host_specs): (Vec<_>, Vec<_>) = config
            .mounts
            .as_ref()
            .map(|ms| {
                ms.iter()
                    .partition(|m| smolvm::remote_volume::is_remote_source(&m.source))
            })
            .unwrap_or_default();
        let mounts: Vec<smolvm::agent::HostMount> = host_specs
            .into_iter()
            .map(smolvm::agent::HostMount::try_from)
            .collect::<smolvm::Result<_>>()
            .into_napi()?;
        let remote_volumes: Vec<_> = remote_specs
            .into_iter()
            .map(|m| {
                if m.staged.unwrap_or(false) {
                    return Err(smolvm::Error::invalid_mount_path(
                        "staged mode is only supported for local host directories",
                    ));
                }
                smolvm::remote_volume::from_parts(
                    &m.source,
                    &m.target,
                    m.read_only.unwrap_or(false),
                )
            })
            .collect::<smolvm::Result<_>>()
            .into_napi()?;

        let ports = config
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .map(smolvm::data::network::PortMapping::from)
                    .collect()
            })
            .unwrap_or_default();

        let resources = config
            .resources
            .as_ref()
            .map(|r| r.to_vm_resources())
            .transpose()?
            .unwrap_or_default();
        // Hostname rules live on the spec, not in VmResources: the engine
        // persists them separately and its DNS filter enforces them at runtime.
        let allowed_hosts = config
            .resources
            .as_ref()
            .and_then(|r| r.allowed_hosts.clone())
            .unwrap_or_default();

        let env = config
            .env
            .unwrap_or_default()
            .into_iter()
            .map(|item| (item.key, item.value))
            .collect();
        let workdir = config.workdir;
        let user = config.user;
        let (credentials, credential_values) =
            credential_policy(config.credentials.unwrap_or_default());
        let spec = MachineSpec {
            name: config.name.clone(),
            mounts,
            ports,
            resources,
            image: config.image.clone(),
            persistent: config.persistent.unwrap_or(false),
            forkable: config.forkable.unwrap_or(false),
            detached: config.detached.unwrap_or(false),
            labels: config.labels.unwrap_or_default().into_iter().collect(),
            runtime_managed: false,
            remote_volumes,
            allowed_hosts,
            credentials,
            ..Default::default()
        };

        let runtime = runtime().into_napi()?;
        runtime
            .create_machine_with_workload(spec, env, workdir, user)
            .into_napi()?;
        // Held in memory for this machine's starts in this process; a binding
        // without a value is read from this process's environment instead.
        if !credential_values.is_empty() {
            runtime.supply_credential_values(&config.name, credential_values);
        }

        Ok(Self { name: config.name })
    }

    /// Attach to an existing machine by name, starting it if stopped
    /// (start-or-reconnect). Lets a persisted machine be re-opened in a new
    /// process — backs the SDK's local `Machine.connect()`.
    #[napi(factory)]
    pub fn connect(
        name: String,
        interceptor_address: Option<String>,
        interceptor_token: Option<String>,
    ) -> napi::Result<Self> {
        let interceptor = parse_interceptor(interceptor_address, interceptor_token)?;
        let runtime = runtime().into_napi()?;
        // A saved execution must only be restored via resume. Expose its
        // existing record so callers can inspect or resume it explicitly;
        // connect_or_start would correctly refuse to boot over its checkpoint.
        let has_saved_execution = matches!(runtime.state(&name).as_str(), "paused" | "pausing")
            && runtime
                .list_machines()
                .into_napi()?
                .iter()
                .any(|record| record.name == name && record.paused_checkpoint.is_some());
        if has_saved_execution {
            if interceptor.is_some() {
                return Err(napi::Error::from_reason(
                    "cannot bind an egress interceptor to a paused machine; resume does not install a new binding",
                ));
            }
            return Ok(Self { name });
        }
        runtime
            .connect_or_start_machine_with_interceptor(&name, interceptor)
            .into_napi()?;
        Ok(Self { name })
    }

    /// Every machine in this host's shared machine database, in name order.
    /// The database is shared with the CLI and every other embedder on the
    /// host, so machines other processes made are listed too — `labels` tell
    /// a process's own apart after it restarts.
    #[napi]
    pub async fn list() -> napi::Result<Vec<MachineSummary>> {
        let runtime = runtime().into_napi()?;
        tokio::task::spawn_blocking(move || {
            let summaries = runtime
                .list_machines()?
                .into_iter()
                .map(|record| {
                    let state = runtime.state(&record.name);
                    // The record keeps its last PID after the process is gone;
                    // report one only while the state says it is alive.
                    let pid = record.pid.filter(|_| state == "running");
                    MachineSummary {
                        state,
                        pid,
                        image: record.image.clone(),
                        labels: record
                            .labels
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                        persistent: !record.ephemeral,
                        detached: record.detached,
                        branchable: record.forkable_on_start(),
                        created_at: record.created_at as f64,
                        name: record.name,
                    }
                })
                .collect();
            Ok(summaries)
        })
        .await
        .map_err(join_error)?
        .into_napi()
    }

    /// Create a stopped machine from a portable live checkpoint on disk.
    /// `keep_identity` restores it as the same machine going back in time, so
    /// its first start skips the identity reset a clone needs.
    #[napi(factory)]
    pub fn restore_checkpoint(
        name: String,
        artifact: String,
        keep_identity: Option<bool>,
    ) -> napi::Result<Self> {
        runtime()
            .into_napi()?
            .restore_checkpoint_machine_with(
                &name,
                std::path::Path::new(&artifact),
                smolvm::embedded::RestoreOptions {
                    keep_identity: keep_identity.unwrap_or(false),
                },
            )
            .into_napi()?;
        Ok(Self { name })
    }

    /// Replace this stopped machine's outbound network policy, e.g. to give a
    /// machine just restored from a shared checkpoint its own allow list
    /// before it first boots.
    #[napi]
    pub fn set_egress_policy(&self, policy: EgressPolicyConfig) -> napi::Result<()> {
        runtime()
            .into_napi()?
            .set_egress_policy(
                &self.name,
                &smolvm::data::network::EgressPolicy {
                    network: policy.network,
                    cidrs: policy.cidrs.unwrap_or_default(),
                    hosts: policy.hosts.unwrap_or_default(),
                },
            )
            .into_napi()
    }

    /// Export a stored checkpoint directory as one portable checkpoint file.
    #[napi]
    pub fn export_checkpoint(source: String, output: String) -> napi::Result<f64> {
        smolvm::checkpoint_store::export(
            std::path::Path::new(&source),
            std::path::Path::new(&output),
        )
        .map(|bytes| bytes as f64)
        .map_err(|error| napi::Error::from_reason(format!("export checkpoint: {error}")))
    }

    /// Remove objects that no retained checkpoint in a local store references.
    #[napi]
    pub fn prune_checkpoint_store(store: String) -> napi::Result<f64> {
        smolvm::checkpoint_store::prune(std::path::Path::new(&store))
            .map(|bytes| bytes as f64)
            .map_err(|error| napi::Error::from_reason(format!("prune checkpoint store: {error}")))
    }

    /// Get the machine name.
    #[napi(getter)]
    pub fn name(&self) -> String {
        self.name.clone()
    }

    /// Start a command and return a live `ExecStream` of its output events. A
    /// worker thread drives the engine's `exec_streaming_with` and feeds an mpsc
    /// channel the stream drains incrementally (no buffering).
    #[napi]
    pub fn exec_stream(&self, command: Vec<String>, options: Option<ExecOptions>) -> ExecStream {
        let name = self.name.clone();
        let options = parse_exec_options(options);
        let cancel = smolvm::embedded::ExecCancel::new();
        let worker_cancel = cancel.clone();
        let (tx, rx) = std::sync::mpsc::channel::<ExecEvent>();
        let err_tx = tx.clone();
        std::thread::spawn(move || {
            let result = runtime().and_then(|rt| {
                rt.exec_streaming_with_options(&name, command, options, &worker_cancel, move |ev| {
                    let _ = tx.send(ev);
                })
            });
            if let Err(e) = result {
                let _ = err_tx.send(ExecEvent::Error(e.to_string()));
            }
            // Senders drop here → channel closes → next() resolves null.
        });
        ExecStream {
            rx: std::sync::Arc::new(std::sync::Mutex::new(rx)),
            cancel,
        }
    }

    /// Get the child PID if the VM is running.
    #[napi(getter)]
    pub fn pid(&self) -> Option<i32> {
        runtime().ok().and_then(|runtime| runtime.pid(&self.name))
    }

    /// Check if the VM process is currently running.
    #[napi(getter)]
    pub fn is_running(&self) -> bool {
        runtime()
            .map(|runtime| runtime.is_running(&self.name))
            .unwrap_or(false)
    }

    /// Get the current machine state: "stopped", "starting", "running", or "stopping".
    #[napi]
    pub fn state(&self) -> String {
        runtime()
            .map(|runtime| runtime.state(&self.name))
            .unwrap_or_else(|_| "stopped".to_string())
    }

    /// Return the host port forwarding to a published guest port.
    #[napi]
    pub fn host_port(&self, guest_port: u16) -> napi::Result<Option<u16>> {
        runtime()
            .into_napi()?
            .host_port(&self.name, guest_port)
            .into_napi()
    }

    /// Return every published guest port.
    #[napi]
    pub fn guest_ports(&self) -> napi::Result<Vec<u16>> {
        runtime().into_napi()?.guest_ports(&self.name).into_napi()
    }

    /// Start the machine VM. Boots via fork + libkrun, waits for agent ready,
    /// then connects the vsock client.
    #[napi]
    pub async fn start(
        &self,
        interceptor_address: Option<String>,
        interceptor_token: Option<String>,
    ) -> napi::Result<()> {
        let interceptor = parse_interceptor(interceptor_address, interceptor_token)?;
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();
        tokio::task::spawn_blocking(move || {
            runtime.start_machine_with_interceptor(&name, interceptor)
        })
        .await
        .map_err(join_error)?
        .into_napi()
    }

    /// Start this machine as a forkable fork base (memfd-backed guest RAM +
    /// control socket) so it can later be `fork()`-ed.
    #[napi]
    pub async fn start_forkable(&self) -> napi::Result<()> {
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();
        tokio::task::spawn_blocking(move || runtime.start_forkable_machine(&name))
            .await
            .map_err(join_error)?
            .into_napi()
    }

    /// Capture this running checkpointable machine to local disk.
    #[napi]
    pub async fn checkpoint(
        &self,
        output: String,
        store: Option<String>,
    ) -> napi::Result<LocalCheckpointResult> {
        let name = self.name.clone();
        let result = tokio::task::spawn_blocking(move || {
            let options = smolvm::portable_checkpoint::CaptureOptions {
                store_dir: store.map(std::path::PathBuf::from),
                rootfs_dir: Some(smolvm::agent::AgentManager::default_rootfs_path()?),
                ..Default::default()
            };
            runtime()?.checkpoint_machine(&name, std::path::Path::new(&output), &options)
        })
        .await
        .map_err(join_error)?
        .into_napi()?;
        Ok(LocalCheckpointResult {
            size_bytes: result.size_bytes as f64,
            reused_bytes: result.reused_bytes as f64,
            source_pause_ms: result.source_pause.as_secs_f64() * 1000.0,
            elapsed_ms: result.elapsed.as_secs_f64() * 1000.0,
        })
    }

    /// Grow this running machine without rebooting it. Sizes are totals.
    /// Every target is checked before anything changes, then RAM, CPUs and
    /// disks are applied in that order.
    #[napi]
    pub async fn resize(&self, spec: ResizeConfig) -> napi::Result<MachineResources> {
        let steps = resize_steps(&spec)?;
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();
        let record = tokio::task::spawn_blocking(move || {
            check_resize_targets(&name, &steps)?;
            let mut record = None;
            for step in steps {
                record = Some(runtime.resize_machine(&name, step)?);
            }
            Ok::<_, smolvm::Error>(record)
        })
        .await
        .map_err(join_error)?
        .into_napi()?
        .ok_or_else(|| napi::Error::from_reason("resize needs at least one resource"))?;
        Ok(MachineResources {
            cpus: u32::from(record.cpus),
            memory_mib: record.mem,
            storage_gib: record.storage_gb.map(|g| g as f64),
            overlay_gib: record.overlay_gb.map(|g| g as f64),
        })
    }

    /// Fork this running, forkable machine into a new clone via copy-on-write
    /// live RAM + disks (same host). `ports` are `{ host, guest }` inbound
    /// forwards for the clone. Returns a handle to the running clone.
    #[napi]
    pub async fn fork(
        &self,
        name: String,
        ports: Option<Vec<PortMappingConfig>>,
        checkpointable: Option<bool>,
        freeze_source: Option<bool>,
    ) -> napi::Result<NapiMachine> {
        let runtime = runtime().into_napi()?;
        let golden = self.name.clone();
        let clone = name.clone();
        let pinned: Vec<(u16, u16)> = ports
            .unwrap_or_default()
            .iter()
            .map(|p| (p.host, p.guest))
            .collect();
        tokio::task::spawn_blocking(move || {
            runtime.fork_machine_with(
                &golden,
                &clone,
                &pinned,
                checkpointable.unwrap_or(false),
                source_policy(freeze_source),
            )
        })
        .await
        .map_err(join_error)?
        .into_napi()?;
        Ok(NapiMachine { name })
    }

    /// Fork many clones from one retained snapshot and boot them in bounded
    /// parallel waves. Transactional: an error removes every clone in this call.
    #[napi]
    pub async fn fork_batch(
        &self,
        names: Vec<String>,
        ports: Option<Vec<PortMappingConfig>>,
        parallel: Option<u32>,
        freeze_source: Option<bool>,
    ) -> napi::Result<Vec<NapiMachine>> {
        let runtime = runtime().into_napi()?;
        let golden = self.name.clone();
        let clones = names.clone();
        let pinned: Vec<(u16, u16)> = ports
            .unwrap_or_default()
            .iter()
            .map(|p| (p.host, p.guest))
            .collect();
        let width = parallel.unwrap_or(8).max(1) as usize;
        tokio::task::spawn_blocking(move || {
            runtime.fork_machines_with(
                &golden,
                &clones,
                &pinned,
                width,
                source_policy(freeze_source),
            )
        })
        .await
        .map_err(join_error)?
        .into_napi()?;
        Ok(names.into_iter().map(|name| NapiMachine { name }).collect())
    }

    /// Execute a command directly in the VM (not in a container).
    #[napi]
    pub async fn exec(
        &self,
        command: Vec<String>,
        options: Option<ExecOptions>,
    ) -> napi::Result<ExecResult> {
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();
        let options = parse_exec_options(options);

        let result =
            tokio::task::spawn_blocking(move || runtime.exec_with_options(&name, command, options))
                .await
                .map_err(join_error)?
                .into_napi()?;

        Ok(ExecResult {
            exit_code: result.0,
            stdout: String::from_utf8_lossy(&result.1).into_owned(),
            stderr: String::from_utf8_lossy(&result.2).into_owned(),
        })
    }

    /// Pull an OCI image and run a command inside it.
    ///
    /// This pulls the image (if not already cached), creates an overlay rootfs,
    /// runs the command inside it, and cleans up. Equivalent to `smolvm run`.
    #[napi]
    pub async fn run(
        &self,
        image: String,
        command: Vec<String>,
        options: Option<ExecOptions>,
    ) -> napi::Result<ExecResult> {
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();
        let options = parse_exec_options(options);
        if options.user.is_some() {
            return Err(napi::Error::from_reason(
                "[CONFIG_ERROR] user is not supported by run(image, command); \
                 use exec on an image machine",
            ));
        }

        let result = tokio::task::spawn_blocking(move || {
            runtime.run(
                &name,
                &image,
                command,
                options.env,
                options.workdir,
                options.timeout,
            )
        })
        .await
        .map_err(join_error)?
        .into_napi()?;

        Ok(ExecResult {
            exit_code: result.0,
            stdout: String::from_utf8_lossy(&result.1).into_owned(),
            stderr: String::from_utf8_lossy(&result.2).into_owned(),
        })
    }

    /// Pull an OCI image into the machine's storage.
    #[napi]
    pub async fn pull_image(&self, image: String) -> napi::Result<ImageInfo> {
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();

        let info = tokio::task::spawn_blocking(move || runtime.pull_image(&name, &image))
            .await
            .map_err(join_error)?
            .into_napi()?;

        Ok(ImageInfo::from(info))
    }

    /// List all cached OCI images in the machine's storage.
    #[napi]
    pub async fn list_images(&self) -> napi::Result<Vec<ImageInfo>> {
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();

        let images = tokio::task::spawn_blocking(move || runtime.list_images(&name))
            .await
            .map_err(join_error)?
            .into_napi()?;

        Ok(images.into_iter().map(ImageInfo::from).collect())
    }

    /// Write a file into the running VM.
    #[napi]
    pub async fn write_file(
        &self,
        path: String,
        data: Buffer,
        options: Option<FileWriteOptions>,
    ) -> napi::Result<()> {
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();
        let mode = options.and_then(|opts| opts.mode);
        let data = data.to_vec();

        tokio::task::spawn_blocking(move || runtime.write_file(&name, &path, data, mode))
            .await
            .map_err(join_error)?
            .into_napi()
    }

    /// Read a file from the running VM.
    #[napi]
    pub async fn read_file(&self, path: String) -> napi::Result<Buffer> {
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();

        let data = tokio::task::spawn_blocking(move || runtime.read_file(&name, &path))
            .await
            .map_err(join_error)?
            .into_napi()?;

        Ok(data.into())
    }

    /// Execute a command and return streaming stdout/stderr/exit events.
    #[napi]
    pub async fn exec_streaming(
        &self,
        command: Vec<String>,
        options: Option<ExecOptions>,
    ) -> napi::Result<Vec<ExecStreamEvent>> {
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();
        let options = parse_exec_options(options);

        let events = tokio::task::spawn_blocking(move || {
            let mut events = Vec::new();
            runtime
                .exec_streaming_with_options(
                    &name,
                    command,
                    options,
                    &smolvm::embedded::ExecCancel::new(),
                    |e| events.push(e),
                )
                .map(|()| events)
        })
        .await
        .map_err(join_error)?
        .into_napi()?;

        Ok(events.into_iter().map(ExecStreamEvent::from).collect())
    }

    /// Copy guest-local staged mounts back to their host sources.
    #[napi]
    pub async fn sync(&self) -> napi::Result<()> {
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();
        tokio::task::spawn_blocking(move || runtime.sync_machine(&name))
            .await
            .map_err(join_error)?
            .into_napi()
    }

    /// Stop the machine VM gracefully.
    #[napi]
    pub async fn stop(&self) -> napi::Result<()> {
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();
        tokio::task::spawn_blocking(move || runtime.stop_machine(&name))
            .await
            .map_err(join_error)?
            .into_napi()
    }

    /// Save execution durably and stop the machine.
    #[napi]
    pub async fn pause(&self) -> napi::Result<()> {
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();
        tokio::task::spawn_blocking(move || runtime.pause_machine(&name))
            .await
            .map_err(join_error)?
            .into_napi()
    }

    #[napi]
    pub async fn resume(&self) -> napi::Result<()> {
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();
        tokio::task::spawn_blocking(move || runtime.resume_machine(&name))
            .await
            .map_err(join_error)?
            .into_napi()
    }

    /// Stop the machine and clean up all storage (disks, config).
    #[napi]
    pub async fn delete(&self) -> napi::Result<()> {
        let runtime = runtime().into_napi()?;
        let name = self.name.clone();
        tokio::task::spawn_blocking(move || runtime.delete_machine(&name))
            .await
            .map_err(join_error)?
            .into_napi()
    }
}

/// The engine's credential policy for `configs`, and the values supplied with
/// them by binding name.
fn credential_policy(
    configs: Vec<crate::types::CredentialConfig>,
) -> (
    Option<smolvm_protocol::credentials::CredentialPolicy>,
    std::collections::BTreeMap<String, String>,
) {
    if configs.is_empty() {
        return (None, Default::default());
    }
    let mut values = std::collections::BTreeMap::new();
    let credentials = configs
        .into_iter()
        .map(|config| {
            if let Some(value) = config.value {
                values.insert(config.name.clone(), value);
            }
            smolvm_protocol::credentials::CredentialBinding {
                name: config.name,
                environment_variable: config.env_var,
                allowed_hosts: config
                    .hosts
                    .into_iter()
                    .map(|host| host.trim().to_ascii_lowercase())
                    .collect(),
                injection_location: Default::default(),
                methods: config.methods.unwrap_or_else(|| {
                    smolvm_protocol::credentials::DEFAULT_METHODS
                        .iter()
                        .map(|method| method.to_string())
                        .collect()
                }),
            }
        })
        .collect();
    (
        Some(smolvm_protocol::credentials::CredentialPolicy { credentials }),
        values,
    )
}
