//! The operations `smol_call` dispatches: one per smol-node `NapiMachine`
//! method, with arguments and replies as JSON in snake_case.
//!
//! A machine handle is only its name — the engine runtime keeps every machine
//! keyed by name — so most ops take `{"name": ...}`. Exec streams and exec
//! cancellers are the two live objects; they are kept in registries here and
//! referred to from Go by a numeric id.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use smolvm::agent::{ExecEvent, HostMount, VmResources};
use smolvm::data::network::PortMapping;
use smolvm::data::resources::{DEFAULT_MICROVM_CPU_COUNT, DEFAULT_MICROVM_MEMORY_MIB};
use smolvm::embedded::{runtime, EmbeddedRuntime, ExecCancel, MachineSpec, ResizeSpec};

use crate::{code_and_message, CallError, CallResult, Payload, CANCELLED, SMOLVM_ERROR};

pub(crate) fn dispatch(op: &str, args: Value, payload: &mut Payload) -> CallResult {
    match op {
        "configure_runtime_assets" => configure_runtime_assets(parse(args)?),
        "check_host" => Ok(check_host()),
        "create" => create(parse(args)?),
        "connect" => connect(parse(args)?),
        "list" => list(),
        "restore_checkpoint" => restore_checkpoint(parse(args)?),
        "export_checkpoint" => export_checkpoint(parse(args)?),
        "prune_checkpoint_store" => prune_checkpoint_store(parse(args)?),
        "set_egress_policy" => set_egress_policy(parse(args)?),
        "state" => {
            let a: Named = parse(args)?;
            Ok(json!(rt()?.state(&a.name)))
        }
        "pid" => {
            let a: Named = parse(args)?;
            Ok(json!(rt()?.pid(&a.name)))
        }
        "host_port" => {
            let a: HostPortArgs = parse(args)?;
            Ok(json!(rt()?.host_port(&a.name, a.guest_port)?))
        }
        "guest_ports" => {
            let a: Named = parse(args)?;
            Ok(json!(rt()?.guest_ports(&a.name)?))
        }
        "start" => start(parse(args)?),
        "start_forkable" => {
            let a: Named = parse(args)?;
            rt()?.start_forkable_machine(&a.name)?;
            Ok(Value::Null)
        }
        "checkpoint" => checkpoint(parse(args)?),
        "resize" => resize(parse(args)?),
        "fork" => fork(parse(args)?),
        "fork_batch" => fork_batch(parse(args)?),
        "exec" => exec(parse(args)?),
        "run" => run(parse(args)?),
        "pull_image" => {
            let a: ImageArgs = parse(args)?;
            Ok(image_info(rt()?.pull_image(&a.name, &a.image)?))
        }
        "list_images" => {
            let a: Named = parse(args)?;
            let images = rt()?.list_images(&a.name)?;
            Ok(Value::Array(images.into_iter().map(image_info).collect()))
        }
        "write_file" => {
            let a: WriteFileArgs = parse(args)?;
            let data = std::mem::take(&mut payload.input);
            rt()?.write_file(&a.name, &a.path, data, a.mode)?;
            Ok(Value::Null)
        }
        "read_file" => {
            let a: PathArgs = parse(args)?;
            let data = rt()?.read_file(&a.name, &a.path)?;
            let len = data.len();
            payload.output = Some(data);
            Ok(json!({ "len": len }))
        }
        "sync" => named_unit(args, |rt, name| rt.sync_machine(name)),
        "stop" => named_unit(args, |rt, name| rt.stop_machine(name)),
        "pause" => named_unit(args, |rt, name| rt.pause_machine(name)),
        "resume" => named_unit(args, |rt, name| rt.resume_machine(name)),
        "delete" => named_unit(args, |rt, name| rt.delete_machine(name)),
        "stream_open" => stream_open(parse(args)?),
        "stream_next" => stream_next(parse(args)?),
        "stream_kill" => {
            let a: Id = parse(args)?;
            if let Some(stream) = streams().lock().expect("stream registry").get(&a.id) {
                stream.cancel.cancel();
            }
            Ok(Value::Null)
        }
        "stream_close" => {
            let a: Id = parse(args)?;
            if let Some(stream) = streams().lock().expect("stream registry").remove(&a.id) {
                stream.cancel.cancel();
            }
            Ok(Value::Null)
        }
        "cancel_new" => {
            let id = next_id();
            cancels()
                .lock()
                .expect("cancel registry")
                .insert(id, ExecCancel::new());
            Ok(json!(id))
        }
        "cancel" => {
            let a: Id = parse(args)?;
            if let Some(cancel) = cancels().lock().expect("cancel registry").get(&a.id) {
                cancel.cancel();
            }
            Ok(Value::Null)
        }
        "cancel_close" => {
            let a: Id = parse(args)?;
            cancels().lock().expect("cancel registry").remove(&a.id);
            Ok(Value::Null)
        }
        other => Err(CallError::config(format!("unknown op {other:?}"))),
    }
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, CallError> {
    Ok(serde_json::from_value(args)?)
}

fn rt() -> Result<Arc<EmbeddedRuntime>, CallError> {
    Ok(runtime()?)
}

fn named_unit(
    args: Value,
    f: impl FnOnce(&EmbeddedRuntime, &str) -> smolvm::Result<()>,
) -> CallResult {
    let a: Named = parse(args)?;
    f(&*rt()?, &a.name)?;
    Ok(Value::Null)
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct Id {
    id: u64,
}

#[derive(Deserialize)]
struct HostPortArgs {
    name: String,
    guest_port: u16,
}

#[derive(Deserialize)]
struct ImageArgs {
    name: String,
    image: String,
}

#[derive(Deserialize)]
struct PathArgs {
    name: String,
    path: String,
}

#[derive(Deserialize)]
struct WriteFileArgs {
    name: String,
    path: String,
    #[serde(default)]
    mode: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RuntimeAssets {
    boot_binary: Option<String>,
    lib_dir: Option<String>,
    agent_rootfs: Option<String>,
    agent_rootfs_tar: Option<String>,
}

#[derive(Deserialize)]
struct EnvVar {
    key: String,
    value: String,
}

#[derive(Deserialize)]
struct MountConfig {
    source: String,
    target: String,
    #[serde(default)]
    read_only: Option<bool>,
    #[serde(default)]
    staged: Option<bool>,
}

#[derive(Deserialize, Clone, Copy)]
struct PortConfig {
    host: u16,
    guest: u16,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ResourcesConfig {
    cpus: Option<u8>,
    memory_mib: Option<u32>,
    network: Option<bool>,
    allowed_cidrs: Option<Vec<String>>,
    allowed_hosts: Option<Vec<String>>,
    storage_gib: Option<u64>,
    overlay_gib: Option<u64>,
    gpu: Option<bool>,
    gpu_vram_mib: Option<u32>,
    cuda: Option<bool>,
    network_backend: Option<String>,
}

#[derive(Deserialize)]
struct MachineConfig {
    name: String,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    env: Vec<EnvVar>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    mounts: Vec<MountConfig>,
    #[serde(default)]
    ports: Vec<PortConfig>,
    #[serde(default)]
    resources: ResourcesConfig,
    #[serde(default)]
    persistent: bool,
    #[serde(default)]
    forkable: bool,
    #[serde(default)]
    detached: bool,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Interceptor {
    interceptor_address: Option<String>,
    interceptor_token: Option<String>,
}

#[derive(Deserialize)]
struct StartArgs {
    name: String,
    #[serde(flatten)]
    interceptor: Interceptor,
}

#[derive(Deserialize)]
struct RestoreArgs {
    name: String,
    artifact: String,
    #[serde(default)]
    keep_identity: bool,
}

#[derive(Deserialize)]
struct ExportArgs {
    source: String,
    output: String,
}

#[derive(Deserialize)]
struct StoreArgs {
    store: String,
}

#[derive(Deserialize)]
struct EgressArgs {
    name: String,
    network: bool,
    #[serde(default)]
    cidrs: Vec<String>,
    #[serde(default)]
    hosts: Vec<String>,
}

#[derive(Deserialize)]
struct CheckpointArgs {
    name: String,
    output: String,
    #[serde(default)]
    store: Option<String>,
}

#[derive(Deserialize)]
struct ResizeArgs {
    name: String,
    #[serde(default)]
    cpus: Option<u8>,
    #[serde(default)]
    memory_mib: Option<u32>,
    #[serde(default)]
    storage_gib: Option<u64>,
    #[serde(default)]
    overlay_gib: Option<u64>,
}

#[derive(Deserialize)]
struct ForkArgs {
    name: String,
    clone: String,
    #[serde(default)]
    ports: Vec<PortConfig>,
    #[serde(default)]
    checkpointable: bool,
}

#[derive(Deserialize)]
struct ForkBatchArgs {
    name: String,
    clones: Vec<String>,
    #[serde(default)]
    ports: Vec<PortConfig>,
    #[serde(default)]
    parallel: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ExecOptions {
    env: Vec<EnvVar>,
    workdir: Option<String>,
    timeout_ms: Option<u64>,
    user: Option<String>,
}

#[derive(Deserialize)]
struct ExecArgs {
    name: String,
    command: Vec<String>,
    #[serde(default)]
    options: ExecOptions,
    /// A `cancel_new` id; the call then fails `CANCELLED` once it fires.
    #[serde(default)]
    cancel: Option<u64>,
}

#[derive(Deserialize)]
struct RunArgs {
    name: String,
    image: String,
    command: Vec<String>,
    #[serde(default)]
    options: ExecOptions,
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

fn exec_options(o: ExecOptions) -> Result<smolvm::embedded::ExecOptions, CallError> {
    if o.timeout_ms == Some(0) {
        return Err(CallError::config(
            "an exec timeout must be greater than zero",
        ));
    }
    Ok(smolvm::embedded::ExecOptions {
        env: o.env.into_iter().map(|v| (v.key, v.value)).collect(),
        workdir: o.workdir,
        timeout: o.timeout_ms.map(Duration::from_millis),
        user: o.user,
        ..Default::default()
    })
}

fn vm_resources(r: &ResourcesConfig) -> Result<VmResources, CallError> {
    let network_backend = match r.network_backend.as_deref() {
        None => None,
        Some("tsi") => Some(smolvm::network::NetworkBackend::Tsi),
        Some("virtio-net") => Some(smolvm::network::NetworkBackend::VirtioNet),
        Some(other) => {
            return Err(CallError::config(format!(
                "network_backend must be \"tsi\" or \"virtio-net\", not {other:?}"
            )))
        }
    };
    Ok(VmResources {
        network_backend,
        cpus: r.cpus.unwrap_or(DEFAULT_MICROVM_CPU_COUNT),
        memory_mib: r.memory_mib.unwrap_or(DEFAULT_MICROVM_MEMORY_MIB),
        network: r.network.unwrap_or(false),
        allowed_cidrs: r.allowed_cidrs.clone(),
        storage_gib: r.storage_gib,
        overlay_gib: r.overlay_gib,
        gpu: r.gpu.unwrap_or(false),
        gpu_vram_mib: r.gpu_vram_mib,
        cuda: r.cuda.unwrap_or(false),
        ..Default::default()
    })
}

fn pinned_ports(ports: &[PortConfig]) -> Vec<(u16, u16)> {
    ports.iter().map(|p| (p.host, p.guest)).collect()
}

fn interceptor(
    i: Interceptor,
) -> Result<Option<smolvm_protocol::InterceptEndpoint>, CallError> {
    match (i.interceptor_address, i.interceptor_token) {
        (None, None) => Ok(None),
        (Some(address), Some(token)) => {
            Ok(Some(smolvm::embedded::interceptor_endpoint(&address, &token)?))
        }
        _ => Err(CallError::config(
            "egress interceptor requires both address and token",
        )),
    }
}

fn image_info(i: smolvm_protocol::ImageInfo) -> Value {
    json!({
        "reference": i.reference,
        "digest": i.digest,
        "size": i.size,
        "architecture": i.architecture,
        "os": i.os,
    })
}

fn exec_result(code: i32, stdout: &[u8], stderr: &[u8]) -> Value {
    json!({
        "exit_code": code,
        "stdout": BASE64.encode(stdout),
        "stderr": BASE64.encode(stderr),
    })
}

fn event_value(ev: ExecEvent) -> Value {
    match ev {
        ExecEvent::Stdout(b) => json!({ "kind": "stdout", "data": BASE64.encode(b) }),
        ExecEvent::Stderr(b) => json!({ "kind": "stderr", "data": BASE64.encode(b) }),
        ExecEvent::Exit(code) => json!({ "kind": "exit", "exit_code": code }),
        ExecEvent::Error(message) => json!({ "kind": "error", "message": message }),
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedAssets {
    boot_binary: Option<PathBuf>,
    lib_dir: Option<PathBuf>,
    agent_rootfs: Option<PathBuf>,
    agent_rootfs_tar: Option<PathBuf>,
}

fn resolve_asset(
    key: &str,
    candidate: Option<String>,
    expect_dir: bool,
) -> Result<Option<PathBuf>, CallError> {
    // An empty value, in the environment or from Go, means unset.
    let Some(path) = std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| candidate.filter(|c| !c.is_empty()).map(PathBuf::from))
    else {
        return Ok(None);
    };
    let resolved = path
        .canonicalize()
        .map_err(|e| CallError::config(format!("resolve {key} '{}': {e}", path.display())))?;
    let is_dir = std::fs::metadata(&resolved)
        .map_err(|e| CallError::config(format!("inspect {key} '{}': {e}", resolved.display())))?
        .is_dir();
    if is_dir != expect_dir {
        let kind = if expect_dir { "directory" } else { "file" };
        return Err(CallError::config(format!(
            "{key} '{}' must be a {kind}",
            resolved.display()
        )));
    }
    Ok(Some(resolved))
}

/// Point the engine at the boot helper, hypervisor libraries and guest rootfs
/// the Go package resolved. A value already in the environment wins, as in
/// smol-node. Must run before the first machine op; repeating it with the
/// same paths is a no-op, with different paths an error.
fn configure_runtime_assets(a: RuntimeAssets) -> CallResult {
    static CONFIGURED: OnceLock<Mutex<Option<ResolvedAssets>>> = OnceLock::new();
    let agent_rootfs = resolve_asset("SMOLVM_AGENT_ROOTFS", a.agent_rootfs, true)?;
    let resolved = ResolvedAssets {
        boot_binary: resolve_asset("SMOLVM_BOOT_BINARY", a.boot_binary, false)?,
        lib_dir: resolve_asset("SMOLVM_LIB_DIR", a.lib_dir, true)?,
        agent_rootfs_tar: if agent_rootfs.is_none() {
            resolve_asset("SMOLVM_AGENT_ROOTFS_TAR", a.agent_rootfs_tar, false)?
        } else {
            None
        },
        agent_rootfs,
    };
    let mut configured = CONFIGURED
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| CallError::config("runtime asset configuration lock was poisoned"))?;
    if let Some(existing) = configured.as_ref() {
        if existing != &resolved {
            return Err(CallError::config(
                "runtime assets were already configured with different paths",
            ));
        }
        return Ok(Value::Null);
    }
    for (key, value) in [
        ("SMOLVM_BOOT_BINARY", &resolved.boot_binary),
        ("SMOLVM_LIB_DIR", &resolved.lib_dir),
        ("SMOLVM_AGENT_ROOTFS", &resolved.agent_rootfs),
        ("SMOLVM_AGENT_ROOTFS_TAR", &resolved.agent_rootfs_tar),
    ] {
        if let Some(value) = value {
            std::env::set_var(key, value);
        }
    }
    *configured = Some(resolved);
    Ok(Value::Null)
}

/// Whether this host can run local machines, answered without booting one.
/// Never fails: an unavailable host is a reply, not an error.
fn check_host() -> Value {
    let unavailable = |e: smolvm::Error| {
        let (code, reason) = code_and_message(&e);
        json!({ "available": false, "code": code, "reason": reason })
    };
    #[cfg(target_os = "linux")]
    if let Err(e) = smolvm::platform::linux::check_kvm_available() {
        return unavailable(e);
    }
    match smolvm::vm::backend::LibkrunBackend::new() {
        Ok(backend) if smolvm::vm::VmBackend::is_available(&backend) => {
            json!({ "available": true })
        }
        Ok(_) => unavailable(smolvm::Error::HypervisorUnavailable(
            "libkrun was not found; reinstall the SDK or set SMOLVM_LIB_DIR".into(),
        )),
        Err(e) => unavailable(e),
    }
}

fn create(c: MachineConfig) -> CallResult {
    let mut mounts = Vec::new();
    let mut remote_volumes = Vec::new();
    for m in c.mounts {
        let staged = m.staged.unwrap_or(false);
        // An `s3://` source is mounted inside the guest by the agent, not
        // bind-mounted from the host, so it skips host-directory validation.
        if smolvm::remote_volume::is_remote_source(&m.source) {
            if staged {
                return Err(smolvm::Error::invalid_mount_path(
                    "staged mode is only supported for local host directories",
                )
                .into());
            }
            remote_volumes.push(smolvm::remote_volume::from_parts(
                &m.source,
                &m.target,
                m.read_only.unwrap_or(false),
            )?);
            continue;
        }
        // Writable unless asked otherwise, like `smol -v` and smol-node.
        let read_only = m.read_only.unwrap_or(false);
        if read_only && staged {
            return Err(smolvm::Error::invalid_mount_path(
                "a staged mount is writable and cannot also be read-only",
            )
            .into());
        }
        let mut mount = HostMount::new(&m.source, &m.target, read_only)?;
        mount.staged = staged;
        mounts.push(mount);
    }
    let resources = vm_resources(&c.resources)?;
    let spec = MachineSpec {
        name: c.name,
        mounts,
        ports: c
            .ports
            .iter()
            .map(|p| PortMapping::new(p.host, p.guest))
            .collect(),
        resources,
        image: c.image,
        persistent: c.persistent,
        forkable: c.forkable,
        detached: c.detached,
        labels: c.labels,
        runtime_managed: false,
        remote_volumes,
        // Hostname rules live on the spec, not in VmResources: the engine
        // persists them separately and its DNS filter enforces them.
        allowed_hosts: c.resources.allowed_hosts.unwrap_or_default(),
        ..Default::default()
    };
    let env = c.env.into_iter().map(|v| (v.key, v.value)).collect();
    rt()?.create_machine_with_workload(spec, env, c.workdir, c.user)?;
    Ok(Value::Null)
}

fn connect(a: StartArgs) -> CallResult {
    let interceptor = interceptor(a.interceptor)?;
    rt()?.connect_or_start_machine_with_interceptor(&a.name, interceptor)?;
    Ok(Value::Null)
}

fn start(a: StartArgs) -> CallResult {
    let interceptor = interceptor(a.interceptor)?;
    rt()?.start_machine_with_interceptor(&a.name, interceptor)?;
    Ok(Value::Null)
}

/// Every machine in this host's shared machine database, in name order — the
/// CLI's and other embedders' machines included; `labels` tell them apart.
fn list() -> CallResult {
    let runtime = rt()?;
    let machines = runtime
        .list_machines()?
        .into_iter()
        .map(|record| {
            let state = runtime.state(&record.name);
            // The record keeps its last PID after the process is gone; report
            // one only while the state says it is alive.
            let pid = record.pid.filter(|_| state == "running");
            json!({
                "name": record.name,
                "state": state,
                "image": record.image,
                "labels": record.labels,
                "pid": pid,
                "persistent": !record.ephemeral,
                "detached": record.detached,
                "branchable": record.forkable_on_start(),
                "created_at": record.created_at,
            })
        })
        .collect();
    Ok(Value::Array(machines))
}

fn restore_checkpoint(a: RestoreArgs) -> CallResult {
    rt()?.restore_checkpoint_machine_with(
        &a.name,
        Path::new(&a.artifact),
        smolvm::embedded::RestoreOptions {
            keep_identity: a.keep_identity,
        },
    )?;
    Ok(Value::Null)
}

fn export_checkpoint(a: ExportArgs) -> CallResult {
    let bytes = smolvm::checkpoint_store::export(Path::new(&a.source), Path::new(&a.output))
        .map_err(|e| CallError::new(SMOLVM_ERROR, format!("export checkpoint: {e}")))?;
    Ok(json!(bytes))
}

fn prune_checkpoint_store(a: StoreArgs) -> CallResult {
    let bytes = smolvm::checkpoint_store::prune(Path::new(&a.store))
        .map_err(|e| CallError::new(SMOLVM_ERROR, format!("prune checkpoint store: {e}")))?;
    Ok(json!(bytes))
}

fn set_egress_policy(a: EgressArgs) -> CallResult {
    rt()?.set_egress_policy(
        &a.name,
        &smolvm::data::network::EgressPolicy {
            network: a.network,
            cidrs: a.cidrs,
            hosts: a.hosts,
        },
    )?;
    Ok(Value::Null)
}

fn checkpoint(a: CheckpointArgs) -> CallResult {
    let options = smolvm::portable_checkpoint::CaptureOptions {
        store_dir: a.store.map(PathBuf::from),
        rootfs_dir: Some(smolvm::agent::AgentManager::default_rootfs_path()?),
        ..Default::default()
    };
    let result = rt()?.checkpoint_machine(&a.name, Path::new(&a.output), &options)?;
    Ok(json!({
        "size_bytes": result.size_bytes,
        "reused_bytes": result.reused_bytes,
        "source_pause_ms": result.source_pause.as_secs_f64() * 1000.0,
        "elapsed_ms": result.elapsed.as_secs_f64() * 1000.0,
    }))
}

/// Grow a running machine without rebooting it. Sizes are totals. Every
/// target is checked before anything changes, then RAM, CPUs and disks are
/// applied in that order (RAM's headroom check is the likeliest refusal).
fn resize(a: ResizeArgs) -> CallResult {
    let mut steps = Vec::new();
    if let Some(memory_mib) = a.memory_mib {
        steps.push(ResizeSpec {
            memory_mib: Some(memory_mib),
            ..Default::default()
        });
    }
    if let Some(cpus) = a.cpus {
        steps.push(ResizeSpec {
            cpus: Some(cpus),
            ..Default::default()
        });
    }
    if a.storage_gib.is_some() || a.overlay_gib.is_some() {
        steps.push(ResizeSpec {
            storage_gib: a.storage_gib,
            overlay_gib: a.overlay_gib,
            ..Default::default()
        });
    }
    if steps.is_empty() {
        return Err(CallError::config(
            "resize needs at least one of cpus, memory, storage or overlay",
        ));
    }
    let runtime = rt()?;
    smolvm::agent::live_resize::check_targets(
        &smolvm::db::SmolvmDb::open()?,
        &a.name,
        a.cpus,
        a.memory_mib,
        a.storage_gib,
        a.overlay_gib,
    )?;
    let mut record = None;
    for step in steps {
        record = Some(runtime.resize_machine(&a.name, step)?);
    }
    let record = record.expect("at least one resize step");
    Ok(json!({
        "cpus": record.cpus,
        "memory_mib": record.mem,
        "storage_gib": record.storage_gb,
        "overlay_gib": record.overlay_gb,
    }))
}

fn fork(a: ForkArgs) -> CallResult {
    let pinned = pinned_ports(&a.ports);
    let runtime = rt()?;
    if a.checkpointable {
        runtime.fork_checkpointable_machine(&a.name, &a.clone, &pinned)?;
    } else {
        runtime.fork_machine(&a.name, &a.clone, &pinned)?;
    }
    Ok(Value::Null)
}

/// Fork many clones from one snapshot and boot them in bounded parallel waves.
/// Transactional: an error removes every clone this call made.
fn fork_batch(a: ForkBatchArgs) -> CallResult {
    let pinned = pinned_ports(&a.ports);
    let width = a.parallel.unwrap_or(8).max(1);
    rt()?.fork_machines(&a.name, &a.clones, &pinned, width)?;
    Ok(Value::Null)
}

/// Run a command and wait for it. With a `cancel` id, `cancel` from another
/// goroutine kills it and this call fails `CANCELLED`.
fn exec(a: ExecArgs) -> CallResult {
    let options = exec_options(a.options)?;
    let runtime = rt()?;
    let Some(cancel_id) = a.cancel else {
        let (code, stdout, stderr) = runtime.exec_with_options(&a.name, a.command, options)?;
        return Ok(exec_result(code, &stdout, &stderr));
    };
    let cancel = cancels()
        .lock()
        .expect("cancel registry")
        .get(&cancel_id)
        .cloned()
        .ok_or_else(|| CallError::config(format!("unknown canceller {cancel_id}")))?;
    // The streaming path is the cancellable one; gather its output here.
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;
    let mut failure = None;
    runtime.exec_streaming_with_options(&a.name, a.command, options, &cancel, |ev| match ev {
        ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
        ExecEvent::Stderr(b) => stderr.extend_from_slice(&b),
        ExecEvent::Exit(code) => exit_code = Some(code),
        ExecEvent::Error(m) => failure = Some(m),
    })?;
    if cancel.is_cancelled() {
        return Err(CallError::new(CANCELLED, "the command was killed"));
    }
    if let Some(message) = failure {
        return Err(CallError::new(SMOLVM_ERROR, message));
    }
    Ok(exec_result(exit_code.unwrap_or(-1), &stdout, &stderr))
}

fn run(a: RunArgs) -> CallResult {
    let options = exec_options(a.options)?;
    if options.user.is_some() {
        return Err(CallError::config(
            "a per-command user is not supported by run(image, ...); \
             create a machine from the image and exec as the user instead",
        ));
    }
    let (code, stdout, stderr) = rt()?.run(
        &a.name,
        &a.image,
        a.command,
        options.env,
        options.workdir,
        options.timeout,
    )?;
    Ok(exec_result(code, &stdout, &stderr))
}

// ---------------------------------------------------------------------------
// Exec streams and cancellers
// ---------------------------------------------------------------------------

struct Stream {
    rx: Arc<Mutex<Receiver<ExecEvent>>>,
    cancel: ExecCancel,
}

fn streams() -> &'static Mutex<HashMap<u64, Stream>> {
    static STREAMS: OnceLock<Mutex<HashMap<u64, Stream>>> = OnceLock::new();
    STREAMS.get_or_init(Default::default)
}

fn cancels() -> &'static Mutex<HashMap<u64, ExecCancel>> {
    static CANCELS: OnceLock<Mutex<HashMap<u64, ExecCancel>>> = OnceLock::new();
    CANCELS.get_or_init(Default::default)
}

/// Start a command streaming on a worker thread and return the stream's id.
/// `stream_next` drains it; `stream_kill` kills the command.
fn stream_open(a: ExecArgs) -> CallResult {
    let options = exec_options(a.options)?;
    let cancel = ExecCancel::new();
    let worker_cancel = cancel.clone();
    let (tx, rx) = channel::<ExecEvent>();
    let err_tx = tx.clone();
    let (name, command) = (a.name, a.command);
    std::thread::spawn(move || {
        let outcome = runtime().and_then(|rt| {
            rt.exec_streaming_with_options(&name, command, options, &worker_cancel, move |ev| {
                let _ = tx.send(ev);
            })
        });
        if let Err(e) = outcome {
            let (code, msg) = code_and_message(&e);
            let _ = err_tx.send(ExecEvent::Error(format!("[{code}] {msg}")));
        }
        // Senders drop here, so the channel closes and `stream_next` ends.
    });
    let id = next_id();
    streams().lock().expect("stream registry").insert(
        id,
        Stream {
            rx: Arc::new(Mutex::new(rx)),
            cancel,
        },
    );
    Ok(json!(id))
}

/// Block for the stream's next event; `null` once the command has exited (or
/// was killed) and every event was read.
fn stream_next(a: Id) -> CallResult {
    let (rx, cancel) = {
        let streams = streams().lock().expect("stream registry");
        let stream = streams
            .get(&a.id)
            .ok_or_else(|| CallError::config(format!("unknown stream {}", a.id)))?;
        (stream.rx.clone(), stream.cancel.clone())
    };
    if cancel.is_cancelled() {
        return Ok(Value::Null);
    }
    let received = rx.lock().expect("stream receiver").recv();
    match received {
        // A kill that raced this event wins: what the teardown produced is not
        // the command's output.
        Ok(_) if cancel.is_cancelled() => Ok(Value::Null),
        Ok(ev) => Ok(event_value(ev)),
        Err(_) => Ok(Value::Null),
    }
}
