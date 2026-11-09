//! `cua-runtime-smol`: an external runtime provider for Cua.
//!
//! Cua (https://cua.ai) gives agents desktops in Spaces. A Space runs on a
//! local runtime, and a runtime installed separately from cua is a program
//! named `cua-runtime-<name>` that answers a small JSON protocol: one verb per
//! process, the request on stdin, the response on stdout, an `{"error": …}`
//! object and a non-zero exit when it fails. This binary is that program for
//! smol machines: `cua sb create ghcr.io/trycua/linux:24.04 --runtime smol`
//! boots the Space as a smol machine, and `fork` is a live branch of the
//! running machine, memory and all.
//!
//! The provider only manages lifecycle. Everything inside the Space (the
//! desktop, computer use, streaming) goes through cua-spacesd in the guest,
//! which Cua's images carry; the provider publishes its port.
//!
//! Checkpoints are frozen branch bases held by the engine: `checkpoint`
//! branches the Space and freezes the branch, `fork` of a checkpoint branches
//! the frozen base. Both take well under a second, which matters because cua
//! gives every verb but `start` sixty seconds. A portable `.smolcheckpoint`
//! file is written beside it when `CUA_RUNTIME_SMOL_CHECKPOINT_FILES=1`.
//!
//! The engine refuses to delete a machine that is the base of live branches,
//! so a Space deleted while its forks or checkpoints exist is *retired*:
//! hidden from cua, stopped when nothing running depends on it, and deleted
//! by the garbage collector that runs before `list` and `delete` (and on the
//! `gc` verb) once its branches are gone.
//!
//! Protocol: https://cua.ai/docs/cua-sdk/guides/external-runtime-protocol

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smolmachines::{
    BranchOptions, ConnectOptions, ErrorKind, ExecOptions, Machine, MachineState, Port,
};

/// The runtime word people type: `--runtime smol`.
const NAME: &str = "smol";
/// The protocol version this provider speaks.
const PROTOCOL: u32 = 1;
/// The guest port cua-spacesd listens on in Cua's images.
const SPACESD_PORT: u16 = 3211;
/// Label every machine this provider makes carries.
const LABEL_RUNTIME: (&str, &str) = ("cua.runtime", NAME);
/// Label carrying the Space's name (the machine's own name is prefixed).
const LABEL_NAME: &str = "cua.name";
/// Label carrying a checkpoint's name on its frozen base.
const LABEL_CHECKPOINT: &str = "cua.checkpoint";
/// Prefix of the smol machine behind a Space, so Spaces and other machines
/// on the same host cannot collide by name.
const MACHINE_PREFIX: &str = "cua-sb-";
/// Prefix of the frozen base behind a checkpoint.
const CHECKPOINT_PREFIX: &str = "cua-ck-";
/// Images this provider lists in `cua images ls` and the New Space picker.
/// Only images that run cua-spacesd belong here.
const CATALOG: &[(&str, &str, bool)] = &[
    (
        "ghcr.io/trycua/linux:24.04",
        "Linux desktop (Ubuntu 24.04)",
        true,
    ),
    (
        "ghcr.io/trycua/linux:24.04-slim",
        "Linux (Ubuntu 24.04, no desktop apps)",
        false,
    ),
];

// ---------------------------------------------------------------- protocol

/// A failure, as the protocol's error object reports it.
#[derive(Debug)]
struct ProviderError {
    kind: &'static str,
    message: String,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ProviderError {}

type Result<T> = std::result::Result<T, ProviderError>;

fn err(kind: &'static str, message: impl Into<String>) -> ProviderError {
    ProviderError {
        kind,
        message: message.into(),
    }
}

impl From<smolmachines::Error> for ProviderError {
    fn from(e: smolmachines::Error) -> Self {
        let kind = match e.kind() {
            ErrorKind::NotFound => "not_found",
            ErrorKind::NotSupported => "unsupported",
            ErrorKind::Config | ErrorKind::Conflict | ErrorKind::InvalidState => "invalid",
            ErrorKind::KvmUnavailable
            | ErrorKind::HypervisorUnavailable
            | ErrorKind::Connection
            | ErrorKind::Unauthorized => "unavailable",
            _ => "other",
        };
        err(kind, format!("{} ({})", e.message(), e.code()))
    }
}

impl From<std::io::Error> for ProviderError {
    fn from(e: std::io::Error) -> Self {
        err("other", e.to_string())
    }
}

impl From<serde_json::Error> for ProviderError {
    fn from(e: serde_json::Error) -> Self {
        err("invalid", format!("bad request: {e}"))
    }
}

#[derive(Serialize)]
struct Capabilities {
    suspend: bool,
    fork: bool,
    checkpoint: bool,
    exec: bool,
    screenshot: bool,
    ports: bool,
    restrict_network: bool,
    max_cpus: u32,
    max_memory_mb: u64,
}

#[derive(Serialize)]
struct CatalogImage {
    reference: &'static str,
    title: &'static str,
    default: bool,
}

#[derive(Serialize)]
struct Manifest {
    protocol: u32,
    name: &'static str,
    display_name: &'static str,
    description: &'static str,
    kind: &'static str,
    os: Vec<&'static str>,
    arch: Vec<&'static str>,
    handles_images: bool,
    spacesd_port: u16,
    capabilities: Capabilities,
    images: Vec<CatalogImage>,
}

/// What cua sends to `start`. Fields of the built-in runtimes that a provider
/// ignores (`firmware`, `sidecars`, `gpu`, …) are left out and skipped.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StartSpec {
    name: String,
    image: String,
    cpus: u32,
    memory_mb: u64,
    disk_size_gb: Option<u32>,
    ports: Vec<u16>,
    env: BTreeMap<String, String>,
    command: Option<Vec<String>>,
    restrict_network: bool,
    // Present only for a private image. Never logged.
    registry_credentials: Option<Value>,
}

#[derive(Deserialize)]
struct NameReq {
    name: String,
}

#[derive(Deserialize)]
struct ExecReq {
    name: String,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct CheckpointReq {
    name: String,
    checkpoint: String,
}

#[derive(Deserialize)]
struct ForkReq {
    source: String,
    new_name: String,
}

// ------------------------------------------------------------------- state

/// What the provider remembers about a Space it made, beside the machine
/// the engine keeps. Ports live here because the engine does not report a
/// machine's host ports back to a process that did not create it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Instance {
    name: String,
    machine: String,
    image: String,
    /// Guest port → host port on loopback.
    ports: BTreeMap<u16, u16>,
    #[serde(default)]
    spacesd: Option<bool>,
}

/// A checkpoint: a frozen branch base the engine holds, plus what a fork of
/// it needs to come up with the right shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointRecord {
    name: String,
    /// The frozen base machine.
    held: String,
    image: String,
    /// The source's guest ports; a fork gets fresh host ports for them.
    guest_ports: Vec<u16>,
    #[serde(default)]
    spacesd: Option<bool>,
    /// A portable export of the same state, when files are turned on.
    #[serde(default)]
    artifact: Option<PathBuf>,
}

/// Machines cua asked to delete that the engine still needs as branch
/// bases. The garbage collector deletes them once their branches are gone.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Retired {
    machines: Vec<String>,
}

struct State {
    root: PathBuf,
}

impl State {
    fn open() -> Result<Self> {
        let root = match std::env::var_os("CUA_RUNTIME_SMOL_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::home_dir()
                .ok_or_else(|| err("other", "no home directory"))?
                .join(".smol")
                .join("cua-runtime"),
        };
        fs::create_dir_all(root.join("instances"))?;
        fs::create_dir_all(root.join("checkpoints"))?;
        Ok(Self { root })
    }

    fn instance_path(&self, name: &str) -> PathBuf {
        self.root.join("instances").join(format!("{name}.json"))
    }

    fn checkpoint_path(&self, name: &str) -> PathBuf {
        self.root.join("checkpoints").join(format!("{name}.json"))
    }

    fn checkpoint_artifact(&self, name: &str) -> PathBuf {
        self.root
            .join("checkpoints")
            .join(format!("{name}.smolcheckpoint"))
    }

    fn retired_path(&self) -> PathBuf {
        self.root.join("retired.json")
    }

    fn load_instance(&self, name: &str) -> Result<Option<Instance>> {
        read_json(&self.instance_path(name))
    }

    fn save_instance(&self, inst: &Instance) -> Result<()> {
        write_json(&self.instance_path(&inst.name), inst)
    }

    fn forget_instance(&self, name: &str) {
        let _ = fs::remove_file(self.instance_path(name));
    }

    /// Every recorded instance, by name.
    fn instances(&self) -> Result<Vec<Instance>> {
        self.records(&self.root.join("instances"))
    }

    fn load_checkpoint(&self, name: &str) -> Result<Option<CheckpointRecord>> {
        read_json(&self.checkpoint_path(name))
    }

    fn save_checkpoint(&self, rec: &CheckpointRecord) -> Result<()> {
        write_json(&self.checkpoint_path(&rec.name), rec)
    }

    fn forget_checkpoint(&self, name: &str) {
        let _ = fs::remove_file(self.checkpoint_path(name));
    }

    fn checkpoints(&self) -> Result<Vec<CheckpointRecord>> {
        self.records(&self.root.join("checkpoints"))
    }

    fn records<T: for<'de> Deserialize<'de>>(&self, dir: &Path) -> Result<Vec<T>> {
        let mut out = Vec::new();
        let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        paths.sort();
        for path in paths {
            if let Some(rec) = read_json::<T>(&path)? {
                out.push(rec);
            }
        }
        Ok(out)
    }

    fn retired(&self) -> Result<Retired> {
        Ok(read_json(&self.retired_path())?.unwrap_or_default())
    }

    fn retire(&self, machine: &str) -> Result<()> {
        let mut retired = self.retired()?;
        if !retired.machines.iter().any(|m| m == machine) {
            retired.machines.push(machine.to_string());
        }
        write_json(&self.retired_path(), &retired)
    }

    fn save_retired(&self, retired: &Retired) -> Result<()> {
        write_json(&self.retired_path(), retired)
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(|e| {
            err(
                "other",
                format!("corrupt state file {}: {e}", path.display()),
            )
        })?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

// ----------------------------------------------------------------- helpers

/// Checks a name before it becomes part of a machine name or a file name:
/// cua validates its own names, but the protocol is a public surface.
fn valid_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.')
        && !name.starts_with('.');
    if ok {
        Ok(())
    } else {
        Err(err("invalid", format!("invalid name {name:?}")))
    }
}

fn machine_name(name: &str) -> String {
    format!("{MACHINE_PREFIX}{name}")
}

fn checkpoint_machine_name(name: &str) -> String {
    format!("{CHECKPOINT_PREFIX}{name}")
}

fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Whether an image is one of Cua's, which run cua-spacesd. For any other
/// reference the answer is unknown and cua decides how to wait.
fn runs_spacesd(image: &str) -> Option<bool> {
    if image.starts_with("ghcr.io/trycua/") {
        Some(true)
    } else {
        None
    }
}

/// A free loopback TCP port. There is a window between this and the engine
/// binding it; a collision fails the start with a clear engine error.
fn free_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn allocate_ports(guest_ports: &[u16]) -> Result<BTreeMap<u16, u16>> {
    let mut map = BTreeMap::new();
    for &guest in guest_ports {
        if guest == 0 || map.contains_key(&guest) {
            continue;
        }
        map.insert(guest, free_port()?);
    }
    Ok(map)
}

fn status_word(state: MachineState) -> &'static str {
    match state {
        MachineState::Running | MachineState::Started => "running",
        MachineState::Paused | MachineState::Pausing => "paused",
        MachineState::Stopped | MachineState::Stopping => "stopped",
        MachineState::Starting => "provisioning",
        MachineState::Error => "error",
        MachineState::Deleted => "deleted",
        // `Unknown`, and states a newer SDK may add.
        _ => "unknown",
    }
}

fn instance_json(inst: &Instance, state: MachineState) -> Value {
    let ports: BTreeMap<String, u16> = inst
        .ports
        .iter()
        .map(|(g, h)| (g.to_string(), *h))
        .collect();
    json!({
        "name": inst.name,
        "status": status_word(state),
        "endpoints": {"host": "127.0.0.1", "ports": ports},
        "image": {
            "reference": inst.image,
            "os": "linux",
            "arch": host_arch(),
            "spacesd": inst.spacesd,
        },
    })
}

/// A handle on the machine behind an instance, or `not_found` when the
/// engine no longer has it (deleted behind the provider's back).
fn attach(inst: &Instance) -> Result<(Machine, MachineState)> {
    let machine = Machine::attach(&inst.machine)?;
    match machine.state() {
        MachineState::Deleted => Err(err(
            "not_found",
            format!("machine {} behind {} is gone", inst.machine, inst.name),
        )),
        state => Ok((machine, state)),
    }
}

fn load_or_not_found(state: &State, name: &str) -> Result<Instance> {
    state
        .load_instance(name)?
        .ok_or_else(|| err("not_found", format!("no instance named {name}")))
}

/// Machines are started as branch sources so `fork` and `checkpoint` can
/// clone them live. `CUA_RUNTIME_SMOL_BRANCHABLE=0` starts them plain.
fn branchable() -> bool {
    !matches!(
        std::env::var("CUA_RUNTIME_SMOL_BRANCHABLE").as_deref(),
        Ok("0") | Ok("false") | Ok("no")
    )
}

/// Whether `checkpoint` also writes a portable `.smolcheckpoint` file.
fn checkpoint_files() -> bool {
    matches!(
        std::env::var("CUA_RUNTIME_SMOL_CHECKPOINT_FILES").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn boot(machine: &Machine) -> Result<()> {
    if branchable() {
        machine.start_branchable()?;
    } else {
        machine.start()?;
    }
    Ok(())
}

/// Brings an existing instance's machine to running, whatever it was doing.
fn ensure_running(machine: &Machine, state: MachineState) -> Result<MachineState> {
    match state {
        MachineState::Running | MachineState::Started => {}
        MachineState::Paused | MachineState::Pausing => machine.resume()?,
        MachineState::Stopped | MachineState::Stopping => boot(machine)?,
        MachineState::Starting => machine.wait_until_ready()?,
        other => {
            return Err(err(
                "other",
                format!("machine {} is in state {other}", machine.name()),
            ))
        }
    }
    Ok(machine.state())
}

/// Puts a source back the way `ensure_running` found it.
fn restore_state(machine: &Machine, before: MachineState) -> Result<()> {
    match before {
        MachineState::Paused | MachineState::Pausing => machine.pause()?,
        MachineState::Stopped | MachineState::Stopping => machine.stop()?,
        _ => {}
    }
    Ok(())
}

fn needs_branchable(what: &str) -> Result<()> {
    if branchable() {
        Ok(())
    } else {
        Err(err(
            "unsupported",
            format!("{what} is off: CUA_RUNTIME_SMOL_BRANCHABLE=0 starts machines as plain, unbranchable VMs"),
        ))
    }
}

/// Whether an engine refusal is "this machine is a branch base".
fn is_branch_base_refusal(e: &smolmachines::Error) -> bool {
    e.message().contains("fork base")
}

/// Deletes a machine, or retires it when the engine still needs it as a
/// branch base. Retired machines are stopped when nothing running depends
/// on them, so they hold disk but no CPU.
fn delete_or_retire(state: &State, machine: &Machine) -> Result<()> {
    match machine.delete() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) if is_branch_base_refusal(&e) => {
            if matches!(
                machine.state(),
                MachineState::Running | MachineState::Started
            ) {
                // Refused while a running branch depends on it; retried by gc.
                let _ = machine.stop();
            }
            state.retire(machine.name())?;
            eprintln!(
                "{} is the base of live branches; retired until they are gone",
                machine.name()
            );
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Deletes retired machines whose branches have since gone away.
fn gc(state: &State) -> Result<usize> {
    let mut retired = state.retired()?;
    if retired.machines.is_empty() {
        return Ok(0);
    }
    let mut kept = Vec::new();
    let mut freed = 0;
    for name in retired.machines.drain(..) {
        let machine = match Machine::attach(&name) {
            Ok(m) => m,
            Err(_) => {
                freed += 1;
                continue;
            }
        };
        match machine.state() {
            MachineState::Deleted => freed += 1,
            MachineState::Running | MachineState::Started => {
                // Still feeding running branches; a stop would be refused.
                kept.push(name);
            }
            _ => match machine.delete() {
                Ok(()) => freed += 1,
                Err(e) if e.kind() == ErrorKind::NotFound => freed += 1,
                Err(_) => kept.push(name),
            },
        }
    }
    retired.machines = kept;
    state.save_retired(&retired)?;
    Ok(freed)
}

// ------------------------------------------------------------------- verbs

fn info() -> Result<Value> {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(8);
    let max_memory_mb = total_memory_mb().unwrap_or(65_536);
    let manifest = Manifest {
        protocol: PROTOCOL,
        name: NAME,
        display_name: "smol machines",
        description: "Linux microVMs on this machine that branch live, memory included (smolvm)",
        kind: "microvm",
        os: vec!["linux"],
        arch: vec![host_arch()],
        handles_images: true,
        spacesd_port: SPACESD_PORT,
        capabilities: Capabilities {
            suspend: true,
            fork: branchable(),
            checkpoint: branchable(),
            exec: true,
            screenshot: false,
            ports: true,
            restrict_network: true,
            max_cpus: cpus.min(255),
            max_memory_mb,
        },
        images: CATALOG
            .iter()
            .map(|(reference, title, default)| CatalogImage {
                reference,
                title,
                default: *default,
            })
            .collect(),
    };
    Ok(serde_json::to_value(manifest)?)
}

#[cfg(target_os = "linux")]
fn total_memory_mb() -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = meminfo
        .lines()
        .find(|l| l.starts_with("MemTotal:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some(kb / 1024)
}

#[cfg(not(target_os = "linux"))]
fn total_memory_mb() -> Option<u64> {
    let out = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    let bytes: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(bytes / (1024 * 1024))
}

fn doctor() -> Result<Value> {
    let mut missing = Vec::new();
    let mut detail = String::new();
    match smolmachines::local_availability() {
        Ok(()) => {}
        Err(e) => {
            missing.push(match e.kind() {
                ErrorKind::KvmUnavailable => "/dev/kvm".to_string(),
                ErrorKind::HypervisorUnavailable => "Hypervisor.framework".to_string(),
                _ => "a supported host".to_string(),
            });
            detail = e.message().to_string();
        }
    }
    if missing.is_empty() {
        match smolmachines::engine_cli() {
            Ok(cli) => detail = format!("smolvm at {}", cli.display()),
            Err(e) => {
                missing.push("smolvm engine".to_string());
                detail = e.message().to_string();
            }
        }
    }
    Ok(json!({
        "ready": missing.is_empty(),
        "detail": detail,
        "missing": missing,
    }))
}

fn start(state: &State, spec: StartSpec) -> Result<Value> {
    valid_name(&spec.name)?;
    if spec.registry_credentials.is_some() {
        // The engine pulls with the host's registry login; a one-off
        // credential has no path into it from here.
        return Err(err(
            "unsupported",
            "private registry credentials are not supported by the smol runtime; \
             log in to the registry on this host instead",
        ));
    }

    // An instance that exists already (made by an earlier start, or by
    // fork) is brought to running and reported as it is.
    if let Some(inst) = state.load_instance(&spec.name)? {
        match attach(&inst) {
            Ok((machine, st)) => {
                let st = ensure_running(&machine, st)?;
                return Ok(instance_json(&inst, st));
            }
            Err(e) if e.kind == "not_found" => state.forget_instance(&spec.name),
            Err(e) => return Err(e),
        }
    }

    if spec.image.is_empty() {
        return Err(err("invalid", "start needs an image"));
    }
    let mut guest_ports = spec.ports.clone();
    if !guest_ports.contains(&SPACESD_PORT) {
        guest_ports.push(SPACESD_PORT);
    }
    let ports = allocate_ports(&guest_ports)?;

    let mut builder = Machine::builder(machine_name(&spec.name))
        .image(&spec.image)
        .network(!spec.restrict_network)
        .label(LABEL_RUNTIME.0, LABEL_RUNTIME.1)
        .label(LABEL_NAME, &spec.name)
        .branchable(branchable());
    if spec.cpus > 0 {
        builder = builder.cpus(spec.cpus.min(255) as u8);
    }
    if spec.memory_mb > 0 {
        builder = builder.memory_mib(spec.memory_mb.min(u32::MAX as u64) as u32);
    }
    if let Some(gb) = spec.disk_size_gb {
        builder = builder.storage_gib(gb as u64);
    }
    for (guest, host) in &ports {
        builder = builder.port(Port::new(*host, *guest));
    }
    for (k, v) in &spec.env {
        builder = builder.env(k, v);
    }
    if let Some(cmd) = spec.command.as_ref().filter(|c| !c.is_empty()) {
        builder = builder.command(cmd.clone());
    }
    let machine = builder.create()?;
    let inst = Instance {
        name: spec.name.clone(),
        machine: machine.name().to_string(),
        image: spec.image.clone(),
        ports,
        spacesd: runs_spacesd(&spec.image),
    };
    // Recorded before the boot, so a boot that fails leaves a stopped
    // instance `cua sb rm` can clean up rather than an orphan machine.
    state.save_instance(&inst)?;
    if let Err(e) = boot(&machine) {
        eprintln!("start of {} failed: {e}", inst.name);
        return Err(e);
    }
    Ok(instance_json(&inst, machine.state()))
}

fn stop(state: &State, name: &str) -> Result<Value> {
    let inst = load_or_not_found(state, name)?;
    let (machine, st) = attach(&inst)?;
    if !matches!(st, MachineState::Stopped) {
        machine.stop()?;
    }
    Ok(json!({}))
}

fn suspend(state: &State, name: &str) -> Result<Value> {
    let inst = load_or_not_found(state, name)?;
    let (machine, st) = attach(&inst)?;
    match st {
        MachineState::Paused | MachineState::Pausing => {}
        MachineState::Running | MachineState::Started => machine.pause()?,
        other => {
            return Err(err(
                "invalid",
                format!("{name} is {other}; only a running instance can be suspended"),
            ))
        }
    }
    Ok(json!({}))
}

fn resume(state: &State, name: &str) -> Result<Value> {
    let inst = load_or_not_found(state, name)?;
    let (machine, st) = attach(&inst)?;
    let st = ensure_running(&machine, st)?;
    Ok(instance_json(&inst, st))
}

fn delete(state: &State, name: &str) -> Result<Value> {
    let inst = load_or_not_found(state, name)?;
    if let Ok(machine) = Machine::attach(&inst.machine) {
        if machine.state() != MachineState::Deleted {
            delete_or_retire(state, &machine)?;
        }
    }
    state.forget_instance(name);
    let _ = gc(state);
    Ok(json!({}))
}

fn status(state: &State, name: &str) -> Result<Value> {
    let inst = load_or_not_found(state, name)?;
    let (_, st) = attach(&inst)?;
    Ok(json!({"status": status_word(st)}))
}

fn endpoints(state: &State, name: &str) -> Result<Value> {
    let inst = load_or_not_found(state, name)?;
    let ports: BTreeMap<String, u16> = inst
        .ports
        .iter()
        .map(|(g, h)| (g.to_string(), *h))
        .collect();
    Ok(json!({"host": "127.0.0.1", "ports": ports}))
}

/// Only the instances this provider made: its own records, each checked
/// against the engine. A record whose machine is gone is reported `deleted`
/// (cua shows the Space as missing) and dropped.
fn list(state: &State) -> Result<Value> {
    let _ = gc(state);
    let by_name: BTreeMap<String, MachineState> =
        Machine::list(&ConnectOptions::local(), &[LABEL_RUNTIME])?
            .into_iter()
            .map(|row| (row.name, row.state))
            .collect();
    let mut out = Vec::new();
    for inst in state.instances()? {
        let st = match by_name.get(&inst.machine) {
            Some(st) => *st,
            None => Machine::attach(&inst.machine)
                .map(|m| m.state())
                .unwrap_or(MachineState::Deleted),
        };
        if st == MachineState::Deleted {
            state.forget_instance(&inst.name);
        }
        out.push(json!({"name": inst.name, "status": status_word(st)}));
    }
    Ok(Value::Array(out))
}

fn exec(state: &State, req: ExecReq) -> Result<Value> {
    if req.argv.is_empty() {
        return Err(err("invalid", "exec needs argv"));
    }
    let inst = load_or_not_found(state, &req.name)?;
    let (machine, st) = attach(&inst)?;
    if !matches!(st, MachineState::Running | MachineState::Started) {
        return Err(err(
            "invalid",
            format!(
                "{} is {}; exec needs a running instance",
                req.name,
                status_word(st)
            ),
        ));
    }
    let mut options = ExecOptions::new();
    for (k, v) in &req.env {
        options = options.env(k, v);
    }
    // Under cua's own timeout for the call; the engine kills the command.
    options = options.timeout(Duration::from_secs(50));
    let out = machine.exec_with(req.argv, options)?;
    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(json!({
        "exit_code": out.exit_code,
        "stdout_b64": b64.encode(&out.stdout),
        "stderr_b64": b64.encode(&out.stderr),
    }))
}

/// `checkpoint {name, checkpoint}`: a frozen branch of the instance, held
/// by the engine as a branch base. The instance keeps running (a paused one
/// is resumed for the branch and paused again). Under a second.
fn checkpoint(state: &State, req: CheckpointReq) -> Result<Value> {
    valid_name(&req.checkpoint)?;
    needs_branchable("checkpoint")?;
    if state.load_checkpoint(&req.checkpoint)?.is_some() {
        return Err(err(
            "invalid",
            format!("a checkpoint named {} already exists", req.checkpoint),
        ));
    }
    let inst = load_or_not_found(state, &req.name)?;
    let (machine, before) = attach(&inst)?;
    ensure_running(&machine, before)?;

    let held_name = checkpoint_machine_name(&req.checkpoint);
    let held = machine.branch_with(
        &held_name,
        BranchOptions::new()
            .checkpointable(true)
            .label(LABEL_RUNTIME.0, LABEL_RUNTIME.1)
            .label(LABEL_CHECKPOINT, &req.checkpoint),
    )?;
    // Freezing is a property of branching: a throwaway branch with
    // `freeze_source` leaves the held machine frozen as a base.
    let scratch = held.branch_with(
        format!("cua-ckf-{}", req.checkpoint),
        BranchOptions::new().freeze_source(true),
    )?;
    scratch.delete()?;

    let artifact = if checkpoint_files() {
        let path = state.checkpoint_artifact(&req.checkpoint);
        machine.checkpoint(Some(&path))?;
        Some(path)
    } else {
        None
    };
    restore_state(&machine, before)?;
    state.save_checkpoint(&CheckpointRecord {
        name: req.checkpoint,
        held: held_name,
        image: inst.image.clone(),
        guest_ports: inst.ports.keys().copied().collect(),
        spacesd: inst.spacesd,
        artifact,
    })?;
    Ok(json!({}))
}

/// `fork {source, new_name}`: a new instance from an instance or a
/// checkpoint. An instance's name wins over a checkpoint's, as in cua.
///
/// The new instance is a live branch: a copy-on-write clone with the
/// source's memory, processes and open connections, with fresh host ports,
/// up in well under a second. A running source keeps running; a paused or
/// stopped one is brought up for the branch and put back.
fn fork(state: &State, req: ForkReq) -> Result<Value> {
    valid_name(&req.new_name)?;
    needs_branchable("fork")?;
    if state.load_instance(&req.new_name)?.is_some() {
        return Err(err(
            "invalid",
            format!("an instance named {} already exists", req.new_name),
        ));
    }
    let (source, before, image, guest_ports, spacesd) =
        if let Some(src) = state.load_instance(&req.source)? {
            let (machine, before) = attach(&src)?;
            ensure_running(&machine, before)?;
            let guest_ports: Vec<u16> = src.ports.keys().copied().collect();
            (machine, Some(before), src.image, guest_ports, src.spacesd)
        } else if let Some(rec) = state.load_checkpoint(&req.source)? {
            let held = Machine::attach(&rec.held)?;
            if held.state() == MachineState::Deleted {
                return Err(err(
                    "not_found",
                    format!("the machine behind checkpoint {} is gone", req.source),
                ));
            }
            (held, None, rec.image, rec.guest_ports, rec.spacesd)
        } else {
            return Err(err(
                "not_found",
                format!("{} is neither an instance nor a checkpoint", req.source),
            ));
        };

    let ports = allocate_ports(&guest_ports)?;
    let mut options = BranchOptions::new()
        .checkpointable(true)
        .label(LABEL_RUNTIME.0, LABEL_RUNTIME.1)
        .label(LABEL_NAME, &req.new_name);
    for (guest, host) in &ports {
        options = options.port(Port::new(*host, *guest));
    }
    let clone = source.branch_with(machine_name(&req.new_name), options)?;
    state.save_instance(&Instance {
        name: req.new_name.clone(),
        machine: clone.name().to_string(),
        image,
        ports,
        spacesd,
    })?;
    if let Some(before) = before {
        restore_state(&source, before)?;
    }
    Ok(json!({}))
}

// ------------------------------------------------- maintenance (not cua's)

/// `delete-checkpoint {name}`: cua has no verb for it, so a person runs
/// `cua-runtime-smol delete-checkpoint` to free a checkpoint's machine.
fn delete_checkpoint(state: &State, name: &str) -> Result<Value> {
    let rec = state
        .load_checkpoint(name)?
        .ok_or_else(|| err("not_found", format!("no checkpoint named {name}")))?;
    if let Ok(machine) = Machine::attach(&rec.held) {
        if machine.state() != MachineState::Deleted {
            delete_or_retire(state, &machine)?;
        }
    }
    if let Some(path) = &rec.artifact {
        let _ = fs::remove_file(path);
    }
    state.forget_checkpoint(name);
    let _ = gc(state);
    Ok(json!({}))
}

/// `checkpoints {}`: the checkpoints this provider holds.
fn list_checkpoints(state: &State) -> Result<Value> {
    let rows: Vec<Value> = state
        .checkpoints()?
        .into_iter()
        .map(|rec| {
            json!({
                "name": rec.name,
                "machine": rec.held,
                "image": rec.image,
                "file": rec.artifact,
            })
        })
        .collect();
    Ok(Value::Array(rows))
}

// -------------------------------------------------------------------- main

fn run(verb: &str, input: &str) -> Result<Value> {
    let state = State::open()?;
    let parse = |s: &str| -> Result<Value> {
        if s.trim().is_empty() {
            Ok(json!({}))
        } else {
            Ok(serde_json::from_str(s)?)
        }
    };
    let name_of = |v: Value| -> Result<String> { Ok(serde_json::from_value::<NameReq>(v)?.name) };
    match verb {
        "info" => info(),
        "doctor" => doctor(),
        "start" => start(&state, serde_json::from_value(parse(input)?)?),
        "stop" => stop(&state, &name_of(parse(input)?)?),
        "suspend" => suspend(&state, &name_of(parse(input)?)?),
        "resume" => resume(&state, &name_of(parse(input)?)?),
        "delete" => delete(&state, &name_of(parse(input)?)?),
        "status" => status(&state, &name_of(parse(input)?)?),
        "endpoints" => endpoints(&state, &name_of(parse(input)?)?),
        "list" => list(&state),
        "exec" => exec(&state, serde_json::from_value(parse(input)?)?),
        "checkpoint" => checkpoint(&state, serde_json::from_value(parse(input)?)?),
        "fork" => fork(&state, serde_json::from_value(parse(input)?)?),
        "screenshot" => Err(err(
            "unsupported",
            "screenshots come from cua-spacesd in the guest, not from this runtime",
        )),
        // Maintenance, for people: cua never calls these.
        "checkpoints" => list_checkpoints(&state),
        "delete-checkpoint" => delete_checkpoint(&state, &name_of(parse(input)?)?),
        "gc" => Ok(json!({"freed": gc(&state)?})),
        other => Err(err("unsupported", format!("unknown verb {other:?}"))),
    }
}

fn main() -> ExitCode {
    let verb = match std::env::args().nth(1) {
        Some(v) => v,
        None => {
            eprintln!("usage: cua-runtime-{NAME} <verb>   (request as JSON on stdin)");
            return ExitCode::from(2);
        }
    };
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("reading the request: {e}");
        return ExitCode::from(2);
    }
    let mut stdout = std::io::stdout().lock();
    match run(&verb, &input) {
        Ok(value) => {
            let _ = writeln!(stdout, "{value}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(
                stdout,
                "{}",
                json!({"error": {"kind": e.kind, "message": e.message}})
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_matches_the_protocol() {
        let m = info().unwrap();
        assert_eq!(m["protocol"], 1);
        assert_eq!(m["name"], "smol");
        assert_eq!(m["kind"], "microvm");
        assert_eq!(m["handles_images"], true);
        assert_eq!(m["spacesd_port"], 3211);
        assert_eq!(m["capabilities"]["exec"], true);
        assert_eq!(m["capabilities"]["screenshot"], false);
        assert_eq!(m["images"][0]["default"], true);
        assert!(m["images"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["reference"]
                .as_str()
                .unwrap()
                .starts_with("ghcr.io/trycua/")));
    }

    #[test]
    fn start_spec_tolerates_the_builtin_runtimes_fields() {
        let spec: StartSpec = serde_json::from_value(json!({
            "name": "mac", "image": "ghcr.io/trycua/linux:24.04", "os": "linux",
            "cpus": 4, "memory_mb": 8192, "disk_size_gb": null, "ports": [3211],
            "env": {"CUA_ENV_TOKEN": "t"}, "command": null, "ready_timeout_secs": 600,
            "kind": "microvm", "runtime": "smol", "restrict_network": false,
            "probes": [], "ephemeral": false,
            "firmware": "uefi", "sidecars": [], "container_runtime": null, "gpu": null,
            "ready_timeout": {"secs": 600, "nanos": 0}
        }))
        .unwrap();
        assert_eq!(spec.name, "mac");
        assert_eq!(spec.ports, vec![3211]);
        assert_eq!(spec.env["CUA_ENV_TOKEN"], "t");
        assert!(spec.command.is_none());
    }

    #[test]
    fn ports_are_allocated_once_per_guest_port() {
        let ports = allocate_ports(&[3211, 8080, 3211, 0]).unwrap();
        assert_eq!(ports.len(), 2);
        assert!(ports.values().all(|p| *p > 0));
    }

    #[test]
    fn names_are_checked_before_they_become_paths() {
        assert!(valid_name("mac").is_ok());
        assert!(valid_name("dev-1.2_x").is_ok());
        assert!(valid_name("").is_err());
        assert!(valid_name("../x").is_err());
        assert!(valid_name("a b").is_err());
        assert!(valid_name(".hidden").is_err());
    }

    #[test]
    fn machine_names_are_prefixed_and_distinct() {
        assert_eq!(machine_name("dev"), "cua-sb-dev");
        assert_eq!(checkpoint_machine_name("dev"), "cua-ck-dev");
        // A Space named "ck-dev" must not take a checkpoint's machine name.
        assert_ne!(machine_name("ck-dev"), checkpoint_machine_name("dev"));
    }

    #[test]
    fn instance_json_has_the_wire_shape() {
        let inst = Instance {
            name: "mac".into(),
            machine: "cua-sb-mac".into(),
            image: "ghcr.io/trycua/linux:24.04".into(),
            ports: [(3211u16, 50123u16)].into_iter().collect(),
            spacesd: Some(true),
        };
        let v = instance_json(&inst, MachineState::Running);
        assert_eq!(v["status"], "running");
        assert_eq!(v["endpoints"]["host"], "127.0.0.1");
        assert_eq!(v["endpoints"]["ports"]["3211"], 50123);
        assert_eq!(v["image"]["spacesd"], true);
        assert_eq!(v["image"]["os"], "linux");
    }

    #[test]
    fn errors_are_the_protocols_error_object() {
        let e: ProviderError = smolmachines::Error::new(ErrorKind::NotFound, "gone").into();
        assert_eq!(e.kind, "not_found");
        let e: ProviderError = smolmachines::Error::new(ErrorKind::NotSupported, "no").into();
        assert_eq!(e.kind, "unsupported");
        let e: ProviderError = smolmachines::Error::new(ErrorKind::KvmUnavailable, "no kvm").into();
        assert_eq!(e.kind, "unavailable");
        let e: ProviderError = smolmachines::Error::new(ErrorKind::Config, "bad").into();
        assert_eq!(e.kind, "invalid");
    }

    #[test]
    fn branch_base_refusals_are_recognised() {
        let e = smolmachines::Error::new(
            ErrorKind::CommandFailed,
            "delete: machine 'cua-c1' is the fork base for 1 clone(s) (cua-c2); delete the clones first",
        );
        assert!(is_branch_base_refusal(&e));
        assert!(!is_branch_base_refusal(&smolmachines::Error::new(
            ErrorKind::CommandFailed,
            "disk full"
        )));
    }

    #[test]
    fn state_round_trips_through_files() {
        let dir =
            std::env::temp_dir().join(format!("cua-runtime-smol-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::env::set_var("CUA_RUNTIME_SMOL_HOME", &dir);
        let state = State::open().unwrap();
        let inst = Instance {
            name: "x".into(),
            machine: "cua-x".into(),
            image: "img".into(),
            ports: [(3211u16, 1u16)].into_iter().collect(),
            spacesd: None,
        };
        state.save_instance(&inst).unwrap();
        assert_eq!(state.load_instance("x").unwrap().unwrap().machine, "cua-x");
        assert_eq!(state.instances().unwrap().len(), 1);
        state.forget_instance("x");
        assert!(state.load_instance("x").unwrap().is_none());

        state
            .save_checkpoint(&CheckpointRecord {
                name: "ck".into(),
                held: "cua-ck-ck".into(),
                image: "img".into(),
                guest_ports: vec![3211],
                spacesd: Some(true),
                artifact: None,
            })
            .unwrap();
        assert_eq!(state.checkpoints().unwrap()[0].held, "cua-ck-ck");
        state.forget_checkpoint("ck");
        assert!(state.checkpoints().unwrap().is_empty());

        state.retire("cua-old").unwrap();
        state.retire("cua-old").unwrap();
        assert_eq!(state.retired().unwrap().machines, vec!["cua-old"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
