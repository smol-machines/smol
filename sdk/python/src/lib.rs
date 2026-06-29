//! Native (pyo3) core for the smol Python SDK — embeds the smolvm engine.
//!
//! Python analogue of the sibling `smol-node` crate (NAPI). It drives the same
//! `smolvm::embedded` runtime, which is synchronous, so the Python `Machine`
//! API is synchronous too (unlike Node, which must be async).
//!
//! `transport.py`'s `LocalTransport` expects exactly this surface:
//!   Machine(config: dict) / Machine.connect(name) -> instance
//!   .name (property), .state(), .start(), .stop(), .delete()
//!   .exec(command, options) / .run(image, command, options) -> ExecResult
//!   .read_file(path) -> bytes / .write_file(path, data, mode)
//!   .pull_image(image) / .list_images() -> ImageInfo(s)
//!
//! Errors are formatted `"[CODE] message"` so the Python `wrap_native_error`
//! parses them into typed `SmolError`s (parity with smol-node).

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use smolvm::agent::ExecEvent;
use smolvm::embedded::{runtime, MachineSpec};
use smolvm::error::{AgentErrorKind, Error as SmolvmError};

// Error codes exposed to Python as `SmolError.code` (parity with smol-node's
// `errors.rs`). The pure-Python `wrap_native_error` parses the `[CODE]` prefix.
const NOT_FOUND: &str = "NOT_FOUND";
const INVALID_STATE: &str = "INVALID_STATE";
const HYPERVISOR_UNAVAILABLE: &str = "HYPERVISOR_UNAVAILABLE";
const CONFLICT: &str = "CONFLICT";
const STORAGE_ERROR: &str = "STORAGE_ERROR";
const MOUNT_ERROR: &str = "MOUNT_ERROR";
const CONFIG_ERROR: &str = "CONFIG_ERROR";
const COMMAND_FAILED: &str = "COMMAND_FAILED";
const KVM_UNAVAILABLE: &str = "KVM_UNAVAILABLE";
const SMOLVM_ERROR: &str = "SMOLVM_ERROR";

/// Map an engine `smolvm::error::Error` to a `"[CODE] message"` PyErr, mirroring
/// smol-node's `to_napi_error` so both SDKs surface identical error codes. The
/// Python `wrap_native_error` parses the prefix back into a typed `SmolError`.
fn err(e: SmolvmError) -> PyErr {
    let (code, msg) = match &e {
        SmolvmError::VmNotFound { name } => (NOT_FOUND, format!("VM not found: {name}")),
        SmolvmError::InvalidState { expected, actual } => (
            INVALID_STATE,
            format!("Invalid state: expected {expected}, got {actual}"),
        ),
        SmolvmError::HypervisorUnavailable(reason) => (
            HYPERVISOR_UNAVAILABLE,
            format!("Hypervisor unavailable: {reason}"),
        ),
        SmolvmError::Agent {
            operation,
            reason,
            kind,
        } => {
            let code = match kind {
                AgentErrorKind::NotFound => NOT_FOUND,
                AgentErrorKind::Conflict => CONFLICT,
                AgentErrorKind::Other => SMOLVM_ERROR,
            };
            (code, format!("Agent error ({operation}): {reason}"))
        }
        SmolvmError::RootfsNotFound { path } => {
            (NOT_FOUND, format!("Rootfs not found: {}", path.display()))
        }
        SmolvmError::DiskNotFound { path } => {
            (NOT_FOUND, format!("Disk not found: {}", path.display()))
        }
        SmolvmError::MountSourceNotFound { path } => (
            NOT_FOUND,
            format!("Mount source not found: {}", path.display()),
        ),
        SmolvmError::Storage { operation, reason } => {
            (STORAGE_ERROR, format!("Storage ({operation}): {reason}"))
        }
        SmolvmError::Mount { operation, reason } => {
            (MOUNT_ERROR, format!("Mount ({operation}): {reason}"))
        }
        SmolvmError::InvalidMountPath { reason } => {
            (MOUNT_ERROR, format!("Invalid mount path: {reason}"))
        }
        SmolvmError::Config { operation, reason } => {
            (CONFIG_ERROR, format!("Config ({operation}): {reason}"))
        }
        SmolvmError::CommandFailed { command, reason } => (
            COMMAND_FAILED,
            format!("Command '{command}' failed: {reason}"),
        ),
        SmolvmError::KvmUnavailable(reason) => {
            (KVM_UNAVAILABLE, format!("KVM unavailable: {reason}"))
        }
        SmolvmError::KvmPermission(reason) => {
            (KVM_UNAVAILABLE, format!("KVM permission denied: {reason}"))
        }
        _ => (SMOLVM_ERROR, e.to_string()),
    };
    PyRuntimeError::new_err(format!("[{code}] {msg}"))
}

/// Result of a command execution (mirrors smol-node's `ExecResult`).
#[pyclass]
#[derive(Clone)]
struct ExecResult {
    #[pyo3(get)]
    exit_code: i32,
    #[pyo3(get)]
    stdout: String,
    #[pyo3(get)]
    stderr: String,
}

/// Cached OCI image metadata (mirrors smol-node's `ImageInfo`).
#[pyclass]
#[derive(Clone)]
struct ImageInfo {
    #[pyo3(get)]
    reference: String,
    #[pyo3(get)]
    digest: String,
    #[pyo3(get)]
    size: u64,
    #[pyo3(get)]
    architecture: String,
    #[pyo3(get)]
    os: String,
}

fn to_image_info(i: smolvm_protocol::ImageInfo) -> ImageInfo {
    ImageInfo {
        reference: i.reference,
        digest: i.digest,
        size: i.size,
        architecture: i.architecture,
        os: i.os,
    }
}

/// Parse the Python `{env: [{key,value}], workdir, timeout_secs}` options dict
/// into the engine's `(env, workdir, timeout)` triple.
fn parse_exec_opts(
    options: Option<&Bound<'_, PyDict>>,
) -> PyResult<(Vec<(String, String)>, Option<String>, Option<u64>)> {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut workdir: Option<String> = None;
    let mut timeout: Option<u64> = None;
    if let Some(d) = options {
        if let Some(w) = d.get_item("workdir")? {
            if !w.is_none() {
                workdir = Some(w.extract()?);
            }
        }
        if let Some(t) = d.get_item("timeout_secs")? {
            if !t.is_none() {
                timeout = Some(t.extract()?);
            }
        }
        if let Some(e) = d.get_item("env")? {
            if !e.is_none() {
                for item in e.iter()? {
                    let kv = item?;
                    let kv = kv.downcast::<PyDict>()?;
                    let k: String = kv.get_item("key")?.unwrap().extract()?;
                    let v: String = kv.get_item("value")?.unwrap().extract()?;
                    env.push((k, v));
                }
            }
        }
    }
    Ok((env, workdir, timeout))
}

fn lossy(v: Vec<u8>) -> String {
    String::from_utf8_lossy(&v).into_owned()
}

/// Convert an engine `ExecEvent` into a Python dict the SDK layer yields:
/// `{kind: "stdout"|"stderr", data}` / `{kind: "exit", exit_code}` /
/// `{kind: "error", message}`. Mirrors smol-node's `ExecEvent` shape.
fn exec_event_to_dict(py: Python<'_>, ev: ExecEvent) -> PyResult<Py<PyDict>> {
    let d = PyDict::new_bound(py);
    match ev {
        ExecEvent::Stdout(b) => {
            d.set_item("kind", "stdout")?;
            d.set_item("data", lossy(b))?;
        }
        ExecEvent::Stderr(b) => {
            d.set_item("kind", "stderr")?;
            d.set_item("data", lossy(b))?;
        }
        ExecEvent::Exit(code) => {
            d.set_item("kind", "exit")?;
            d.set_item("exit_code", code)?;
        }
        ExecEvent::Error(m) => {
            d.set_item("kind", "error")?;
            d.set_item("message", m)?;
        }
    }
    Ok(d.into())
}

/// A live, incremental exec stream: a Python iterator whose `__next__` blocks
/// (off-GIL) for the next event from a worker thread driving the engine's
/// `exec_streaming_with`. Iteration ends (StopIteration) when the command exits
/// and the channel closes.
#[pyclass]
struct ExecStream {
    // Mutex makes `&Receiver` Send so __next__ can recv inside `allow_threads`.
    rx: std::sync::Mutex<std::sync::mpsc::Receiver<ExecEvent>>,
}

#[pymethods]
impl ExecStream {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Option<Py<PyDict>>> {
        let received = py.allow_threads(|| self.rx.lock().expect("exec stream lock").recv());
        match received {
            Ok(ev) => Ok(Some(exec_event_to_dict(py, ev)?)),
            Err(_) => Ok(None), // channel closed → command finished → StopIteration
        }
    }
}

/// A microVM sandbox handle. Mirrors `smol-node`'s `NapiMachine`.
#[pyclass]
struct Machine {
    name: String,
}

#[pymethods]
impl Machine {
    /// Create (and register) a machine. Does not boot until `start()`.
    #[new]
    fn new(config: &Bound<'_, PyDict>) -> PyResult<Self> {
        let name: String = config
            .get_item("name")?
            .ok_or_else(|| PyRuntimeError::new_err("[INVALID_CONFIG] config['name'] is required"))?
            .extract()?;
        let persistent: bool = match config.get_item("persistent")? {
            Some(v) if !v.is_none() => v.extract()?,
            _ => false,
        };

        // Map the Python `resources` dict → engine `VmResources` (mirrors
        // smol-node's `to_vm_resources`). `network`/`cpus`/`memory_mib` are
        // load-bearing — without `network=true` the guest can't pull images.
        let mut resources = smolvm::agent::VmResources::default();
        if let Some(r) = config.get_item("resources")? {
            if let Ok(rd) = r.downcast::<PyDict>() {
                if let Some(v) = rd.get_item("cpus")? {
                    if !v.is_none() {
                        resources.cpus = v.extract()?;
                    }
                }
                if let Some(v) = rd.get_item("memory_mib")? {
                    if !v.is_none() {
                        resources.memory_mib = v.extract()?;
                    }
                }
                if let Some(v) = rd.get_item("network")? {
                    if !v.is_none() {
                        resources.network = v.extract()?;
                    }
                }
                if let Some(v) = rd.get_item("storage_gib")? {
                    if !v.is_none() {
                        resources.storage_gib = Some(v.extract()?);
                    }
                }
                if let Some(v) = rd.get_item("overlay_gib")? {
                    if !v.is_none() {
                        resources.overlay_gib = Some(v.extract()?);
                    }
                }
                if let Some(v) = rd.get_item("gpu")? {
                    if !v.is_none() {
                        resources.gpu = v.extract()?;
                    }
                }
                if let Some(v) = rd.get_item("gpu_vram_mib")? {
                    if !v.is_none() {
                        resources.gpu_vram_mib = Some(v.extract()?);
                    }
                }
            }
        }
        // Map the Python `mounts` list (dicts of {source, target, read_only})
        // → engine `HostMount` (mirrors smol-node's `HostMount::try_from`).
        // `HostMount::new` validates the paths, so config errors surface here.
        let mut mounts: Vec<smolvm::agent::HostMount> = Vec::new();
        if let Some(m) = config.get_item("mounts")? {
            if !m.is_none() {
                for item in m.iter()? {
                    let d = item?;
                    let d = d.downcast::<PyDict>()?;
                    let source: String = d
                        .get_item("source")?
                        .ok_or_else(|| {
                            PyRuntimeError::new_err("[CONFIG_ERROR] mount requires 'source'")
                        })?
                        .extract()?;
                    let target: String = d
                        .get_item("target")?
                        .ok_or_else(|| {
                            PyRuntimeError::new_err("[CONFIG_ERROR] mount requires 'target'")
                        })?
                        .extract()?;
                    let read_only: bool = match d.get_item("read_only")? {
                        Some(v) if !v.is_none() => v.extract()?,
                        _ => true,
                    };
                    mounts.push(
                        smolvm::agent::HostMount::new(&source, &target, read_only).map_err(err)?,
                    );
                }
            }
        }

        // Map the Python `ports` list (dicts of {host, guest}) → engine `PortMapping`.
        let mut ports: Vec<smolvm::data::network::PortMapping> = Vec::new();
        if let Some(p) = config.get_item("ports")? {
            if !p.is_none() {
                for item in p.iter()? {
                    let d = item?;
                    let d = d.downcast::<PyDict>()?;
                    let host: u16 = d
                        .get_item("host")?
                        .ok_or_else(|| {
                            PyRuntimeError::new_err("[CONFIG_ERROR] port requires 'host'")
                        })?
                        .extract()?;
                    let guest: u16 = d
                        .get_item("guest")?
                        .ok_or_else(|| {
                            PyRuntimeError::new_err("[CONFIG_ERROR] port requires 'guest'")
                        })?
                        .extract()?;
                    ports.push(smolvm::data::network::PortMapping::new(host, guest));
                }
            }
        }

        let spec = MachineSpec {
            name: name.clone(),
            mounts,
            ports,
            resources,
            persistent,
        };
        runtime().map_err(err)?.create_machine(spec).map_err(err)?;
        Ok(Self { name })
    }

    /// Attach to an existing machine by name, starting it if stopped
    /// (start-or-reconnect). Re-opens a persisted machine in a new process —
    /// backs the SDK's local `Machine.connect()`.
    #[staticmethod]
    fn connect(name: String) -> PyResult<Self> {
        runtime().map_err(err)?.start_machine(&name).map_err(err)?;
        Ok(Self { name })
    }

    #[getter]
    fn name(&self) -> String {
        self.name.clone()
    }

    fn state(&self) -> PyResult<String> {
        Ok(runtime().map_err(err)?.state(&self.name))
    }

    fn start(&self) -> PyResult<()> {
        runtime().map_err(err)?.start_machine(&self.name).map_err(err)
    }

    /// Start this machine as a forkable fork base (memfd-backed guest RAM +
    /// control socket) so it can later be `fork()`-ed.
    fn start_forkable(&self) -> PyResult<()> {
        runtime()
            .map_err(err)?
            .start_forkable_machine(&self.name)
            .map_err(err)
    }

    /// Fork this running, forkable machine into a new clone via copy-on-write
    /// live RAM + disks (same host). `ports` are `(host, guest)` inbound forwards
    /// for the clone. Returns a handle to the running clone.
    #[pyo3(signature = (name, ports=None))]
    fn fork(&self, name: String, ports: Option<Vec<(u16, u16)>>) -> PyResult<Self> {
        let pinned = ports.unwrap_or_default();
        runtime()
            .map_err(err)?
            .fork_machine(&self.name, &name, &pinned)
            .map_err(err)?;
        Ok(Machine { name })
    }

    #[pyo3(signature = (command, options=None))]
    fn exec(&self, command: Vec<String>, options: Option<&Bound<'_, PyDict>>) -> PyResult<ExecResult> {
        let (env, workdir, timeout) = parse_exec_opts(options)?;
        let (code, out, errb) = runtime()
            .map_err(err)?
            .exec(&self.name, command, env, workdir, timeout.map(std::time::Duration::from_secs))
            .map_err(err)?;
        Ok(ExecResult { exit_code: code, stdout: lossy(out), stderr: lossy(errb) })
    }

    #[pyo3(signature = (image, command, options=None))]
    fn run(
        &self,
        image: String,
        command: Vec<String>,
        options: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<ExecResult> {
        let (env, workdir, timeout) = parse_exec_opts(options)?;
        let (code, out, errb) = runtime()
            .map_err(err)?
            .run(&self.name, &image, command, env, workdir, timeout.map(std::time::Duration::from_secs))
            .map_err(err)?;
        Ok(ExecResult { exit_code: code, stdout: lossy(out), stderr: lossy(errb) })
    }

    /// Execute a command and stream its output LIVE. Returns an `ExecStream`
    /// iterator yielding `{kind, ...}` dicts as output arrives (no buffering).
    /// A worker thread drives the engine and feeds an mpsc channel; the iterator
    /// blocks off-GIL for each event and stops when the command exits.
    #[pyo3(signature = (command, options=None))]
    fn exec_stream(
        &self,
        command: Vec<String>,
        options: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<ExecStream> {
        let (env, workdir, timeout) = parse_exec_opts(options)?;
        let timeout = timeout.map(std::time::Duration::from_secs);
        let name = self.name.clone();
        let (tx, rx) = std::sync::mpsc::channel::<ExecEvent>();
        let err_tx = tx.clone();
        std::thread::spawn(move || {
            match runtime() {
                Ok(rt) => {
                    if let Err(e) =
                        rt.exec_streaming_with(&name, command, env, workdir, timeout, move |ev| {
                            let _ = tx.send(ev);
                        })
                    {
                        let _ = err_tx.send(ExecEvent::Error(e.to_string()));
                    }
                }
                Err(e) => {
                    let _ = err_tx.send(ExecEvent::Error(e.to_string()));
                }
            }
            // Senders drop here → channel closes → iterator raises StopIteration.
        });
        Ok(ExecStream {
            rx: std::sync::Mutex::new(rx),
        })
    }

    fn read_file<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyBytes>> {
        let data = runtime().map_err(err)?.read_file(&self.name, &path).map_err(err)?;
        Ok(PyBytes::new_bound(py, &data))
    }

    #[pyo3(signature = (path, data, mode=None))]
    fn write_file(&self, path: String, data: Vec<u8>, mode: Option<u32>) -> PyResult<()> {
        runtime().map_err(err)?.write_file(&self.name, &path, data, mode).map_err(err)
    }

    fn pull_image(&self, image: String) -> PyResult<ImageInfo> {
        let i = runtime().map_err(err)?.pull_image(&self.name, &image).map_err(err)?;
        Ok(to_image_info(i))
    }

    fn list_images(&self) -> PyResult<Vec<ImageInfo>> {
        let imgs = runtime().map_err(err)?.list_images(&self.name).map_err(err)?;
        Ok(imgs.into_iter().map(to_image_info).collect())
    }

    fn stop(&self) -> PyResult<()> {
        runtime().map_err(err)?.stop_machine(&self.name).map_err(err)
    }

    fn delete(&self) -> PyResult<()> {
        runtime().map_err(err)?.delete_machine(&self.name).map_err(err)
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Machine>()?;
    m.add_class::<ExecResult>()?;
    m.add_class::<ImageInfo>()?;
    m.add_class::<ExecStream>()?;
    Ok(())
}
