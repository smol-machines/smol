//! Kubernetes lifecycle on persistent Smol machines.
//!
//! The current provisioner manages local clusters; cloud provisioning is rejected
//! before creating a machine until its privileged guest lifecycle is supported.
//! No host service is installed. Dropping a handle leaves the cluster running.

mod artifact;
mod images;

pub use images::ImageImport;

use crate::{
    ConnectOptions, Error, ErrorKind, ExecOptions, ExecResult, Machine, MachineConfig,
    MachineState, NetworkBackend, Port, Resources, Result, Target,
};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const ROOT: &str = "/workspace/.smol/kubernetes";
const BIN: &str = "/workspace/.smol/kubernetes/k3s";
const CONFIG: &str = "/workspace/.smol/kubernetes/kubeconfig";
const KIND: &str = "smol.workload";
const NAME: &str = "smol.kubernetes.name";
const FORMAT: &str = "smol.kubernetes.format";

pub(super) fn failure(kind: ErrorKind, message: impl Into<String>) -> Error {
    Error::new(kind, message)
}
pub(super) fn check_cancel(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(failure(ErrorKind::Other, "Kubernetes operation cancelled"))
    } else {
        Ok(())
    }
}

/// Resources for a single-node development cluster.
#[derive(Debug, Clone)]
pub struct KubernetesConfig {
    /// A lowercase DNS label, at most 48 characters.
    pub name: String,
    /// Guest CPU count (at least two).
    pub cpus: u8,
    /// Guest memory in MiB (at least 2048).
    pub memory_mib: u32,
    /// Persistent disk size in GiB (at least four).
    pub storage_gib: u64,
    /// Host API port; zero selects an available loopback port.
    pub api_port: u16,
}

impl KubernetesConfig {
    /// A cluster with two CPUs, 3 GiB RAM, and an 8 GiB persistent disk.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            cpus: 2,
            memory_mib: 3072,
            storage_gib: 8,
            api_port: 0,
        }
    }
    fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        if self.cpus < 2 || self.memory_mib < 2048 || self.storage_gib < 4 {
            return Err(failure(
                ErrorKind::Config,
                "Kubernetes needs at least 2 CPUs, 2048 MiB RAM and 4 GiB storage",
            ));
        }
        Ok(())
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 48
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || name.starts_with('-')
        || name.ends_with('-')
    {
        return Err(failure(
            ErrorKind::Config,
            "Cluster name must be a lowercase DNS label of 1–48 characters",
        ));
    }
    Ok(())
}

/// Handle to a cluster whose disks and machine survive the calling process.
pub struct Kubernetes {
    name: String,
    machine: Machine,
}

impl Kubernetes {
    /// Create the backing machine without installing or starting Kubernetes.
    /// Call [`Self::start`] next; a failed start retains this handle and its data.
    pub fn create(config: KubernetesConfig) -> Result<Self> {
        Self::create_with(config, &ConnectOptions::local())
    }

    /// Create on an explicit target; unsupported targets fail before creation.
    pub fn create_with(config: KubernetesConfig, connect: &ConnectOptions) -> Result<Self> {
        config.validate()?;
        if connect.target() != Target::Local {
            return Err(failure(
                ErrorKind::NotSupported,
                "Managed Kubernetes cloud provisioning is not available yet",
            ));
        }
        // Reserve distinct loopback ports until the stopped machine is created.
        // The engine owns the bindings once start succeeds and reports collisions.
        let reservations = [config.api_port, 0, 0]
            .into_iter()
            .map(|port| {
                std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).map_err(|e| {
                    failure(ErrorKind::Connection, format!("reserve cluster port: {e}"))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let ports = reservations
            .iter()
            .zip([6443, 80, 443])
            .map(|(socket, guest)| {
                socket
                    .local_addr()
                    .map(|address| Port::new(address.port(), guest))
                    .map_err(|e| failure(ErrorKind::Connection, e.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        let machine = Machine::create_with(
            MachineConfig {
                name: format!("smol-k8s-{}", config.name),
                persistent: true,
                resources: Resources {
                    cpus: Some(config.cpus),
                    memory_mib: Some(config.memory_mib),
                    storage_gib: Some(config.storage_gib),
                    overlay_gib: Some(2),
                    network: Some(true),
                    network_backend: Some(NetworkBackend::VirtioNet),
                    ..Resources::default()
                },
                ports,
                labels: vec![
                    (KIND.into(), "kubernetes".into()),
                    (NAME.into(), config.name.clone()),
                    (FORMAT.into(), "1".into()),
                ],
                ..MachineConfig::default()
            },
            connect,
        )?;
        Ok(Self {
            name: config.name,
            machine,
        })
    }

    /// Attach to a managed local cluster without starting it.
    pub fn attach(name: &str) -> Result<Self> {
        validate_name(name)?;
        let rows = Machine::list(
            &ConnectOptions::local(),
            &[(KIND, "kubernetes"), (NAME, name)],
        )?;
        if rows.len() != 1 {
            return Err(failure(
                if rows.is_empty() {
                    ErrorKind::NotFound
                } else {
                    ErrorKind::Conflict
                },
                format!(
                    "Expected one managed cluster named {name}, found {}",
                    rows.len()
                ),
            ));
        }
        let row = &rows[0];
        if row.labels.get(FORMAT).map(String::as_str) != Some("1") {
            return Err(failure(
                ErrorKind::NotSupported,
                "This cluster uses an unsupported management format",
            ));
        }
        Ok(Self {
            name: name.into(),
            machine: Machine::attach(&row.id)?,
        })
    }

    /// Cluster names on this host, independent of running state.
    pub fn list() -> Result<Vec<String>> {
        Ok(
            Machine::list(&ConnectOptions::local(), &[(KIND, "kubernetes")])?
                .into_iter()
                .filter_map(|m| m.labels.get(NAME).cloned())
                .collect(),
        )
    }

    /// The human-readable cluster name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The backing machine, usable with the regular SDK machine APIs.
    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    fn owned(&self) -> Result<()> {
        let attached = Self::attach(&self.name)?;
        if attached.machine.id() != self.machine.id() {
            return Err(failure(
                ErrorKind::Conflict,
                "Cluster ownership changed; reconnect before changing it",
            ));
        }
        Ok(())
    }

    fn exec(
        &self,
        args: impl IntoIterator<Item = String>,
        timeout: Duration,
    ) -> Result<ExecResult> {
        self.machine.exec_with(
            args,
            ExecOptions::new()
                .timeout(timeout)
                .env("K3S_DATA_DIR", format!("{ROOT}/data")),
        )
    }

    fn checked(&self, args: impl IntoIterator<Item = String>) -> Result<ExecResult> {
        let result = self.exec(args, Duration::from_secs(30))?;
        if !result.success() {
            return Err(failure(
                ErrorKind::CommandFailed,
                format!(
                    "Kubernetes operation failed: {}",
                    result.stderr_utf8().trim()
                ),
            ));
        }
        Ok(result)
    }

    /// Boot and provision, then wait up to `timeout` for the API, policy
    /// controller, DNS, storage provisioner, and ingress deployment.
    /// Cancellation preserves the machine and its disks for a retry or stop.
    pub fn start(&self, timeout: Duration, cancelled: &AtomicBool) -> Result<Value> {
        self.owned()?;
        check_cancel(cancelled)?;
        if timeout.is_zero() {
            return Err(failure(
                ErrorKind::Config,
                "Kubernetes startup timeout must be positive",
            ));
        }
        self.machine.start()?;
        check_cancel(cancelled)?;
        self.checked(["sh", "-c", include_str!("preflight.sh")].map(String::from))?;
        let arch = self.checked(["uname", "-m"].map(String::from))?;
        let artifact = artifact::Artifact::for_arch(arch.stdout_utf8().trim())?;
        let installed = self.exec(["sha256sum".into(), BIN.into()], Duration::from_secs(10))?;
        if !installed.success()
            || installed.stdout_utf8().split_whitespace().next() != Some(artifact.sha256)
        {
            let binary = artifact.ensure(cancelled)?;
            check_cancel(cancelled)?;
            self.checked(["mkdir".into(), "-p".into(), ROOT.into()])?;
            let staging = tempfile::Builder::new()
                .prefix("k3s-")
                .tempfile()
                .map_err(|e| failure(ErrorKind::Storage, e.to_string()))?;
            let staged = format!(
                "{ROOT}/{}",
                staging.path().file_name().unwrap().to_string_lossy()
            );
            self.machine.copy_in(&binary, &staged)?;
            self.checked(
                [
                    "sh",
                    "-c",
                    concat!(include_str!("process.sh"), "\n", include_str!("install.sh")),
                    "smol-kubernetes",
                    &staged,
                    artifact.sha256,
                ]
                .map(String::from),
            )?;
        }
        check_cancel(cancelled)?;
        self.checked(
            [
                "sh",
                "-c",
                concat!(include_str!("process.sh"), "\n", include_str!("start.sh")),
                "smol-kubernetes",
                &self.name,
            ]
            .map(String::from),
        )?;
        let deadline = Instant::now() + timeout;
        loop {
            check_cancel(cancelled)?;
            let status = self.status()?;
            match status["phase"].as_str() {
                Some("ready") => return Ok(status),
                Some("failed" | "stopped" | "paused" | "unavailable") => return Err(failure(ErrorKind::InvalidState,
                    "Kubernetes stopped or rejected its configuration; inspect cluster logs before retrying")),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    ErrorKind::Timeout,
                    "Kubernetes did not become ready before the startup deadline",
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Inspect readiness without starting the machine or downloading artifacts.
    pub fn status(&self) -> Result<Value> {
        let state = self.machine.state();
        if !matches!(state, MachineState::Running | MachineState::Started) {
            return Ok(
                json!({"name":self.name,"machine":self.machine.id(),"phase": match state {
                MachineState::Stopped => "stopped", MachineState::Paused => "paused",
                MachineState::Starting => "starting", _ => "unavailable"
            },"version":artifact::VERSION}),
            );
        }
        let result = self.exec(
            [
                "sh",
                "-c",
                concat!(include_str!("process.sh"), "\n", include_str!("status.sh")),
            ]
            .map(String::from),
            Duration::from_secs(10),
        )?;
        let phase = match result.stdout_utf8().trim() {
            "ready" if result.success() => "ready",
            "failed" => "failed",
            _ => "starting",
        };
        Ok(
            json!({"name":self.name,"machine":self.machine.id(),"phase":phase,"version":artifact::VERSION}),
        )
    }

    /// Run kubectl in the cluster; argument vectors are never interpreted by a shell.
    pub fn kubectl(
        &self,
        args: impl IntoIterator<Item = String>,
        timeout: Duration,
    ) -> Result<ExecResult> {
        self.exec(
            [
                BIN.to_owned(),
                "kubectl".into(),
                "--kubeconfig".into(),
                CONFIG.into(),
            ]
            .into_iter()
            .chain(args),
            timeout,
        )
    }

    /// Read a bounded tail of the cluster's server log for startup diagnostics.
    pub fn logs(&self, lines: u32) -> Result<ExecResult> {
        if lines == 0 || lines > 10_000 {
            return Err(failure(
                ErrorKind::Config,
                "Log lines must be between 1 and 10000",
            ));
        }
        self.exec(
            [
                "tail".into(),
                "-n".into(),
                lines.to_string(),
                format!("{ROOT}/server.log"),
            ],
            Duration::from_secs(10),
        )
    }

    /// Stable loopback endpoints for this cluster's API and ingress controller.
    /// Ports remain reserved in the machine configuration across cold restarts.
    pub fn endpoints(&self) -> Result<Value> {
        let mut result = serde_json::Map::new();
        for (key, scheme, guest) in [
            ("api", "https", 6443),
            ("http", "http", 80),
            ("https", "https", 443),
        ] {
            let host = self
                .machine
                .host_port(guest)?
                .filter(|p| *p != 0)
                .ok_or_else(|| {
                    failure(
                        ErrorKind::NotFound,
                        format!("Cluster port {guest} is not published"),
                    )
                })?;
            result.insert(key.into(), json!(format!("{scheme}://127.0.0.1:{host}")));
        }
        Ok(Value::Object(result))
    }

    /// Return a dedicated kubeconfig; existing host contexts are never modified.
    pub fn kubeconfig(&self) -> Result<Value> {
        let result = self.kubectl(
            ["config", "view", "--raw", "-o", "json"].map(String::from),
            Duration::from_secs(10),
        )?;
        if !result.success() {
            return Err(failure(
                ErrorKind::InvalidState,
                "The cluster kubeconfig is not available yet",
            ));
        }
        let value = serde_json::from_slice(&result.stdout)
            .map_err(|e| failure(ErrorKind::Other, format!("decode cluster kubeconfig: {e}")))?;
        let port = self
            .machine
            .host_port(6443)?
            .filter(|p| *p != 0)
            .ok_or_else(|| {
                failure(
                    ErrorKind::InvalidState,
                    "The cluster has no published API port",
                )
            })?;
        dedicated_config(value, &self.name, port)
    }

    /// Atomically write a private kubeconfig to a new path, refusing to replace a file.
    pub fn write_kubeconfig(&self, path: impl AsRef<Path>) -> Result<()> {
        use std::io::Write;
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent)
            .map_err(|e| failure(ErrorKind::Storage, e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| failure(ErrorKind::Storage, e.to_string()))?;
        }
        let data = serde_json::to_vec_pretty(&self.kubeconfig()?)
            .map_err(|e| failure(ErrorKind::Other, e.to_string()))?;
        file.write_all(&data)
            .and_then(|()| file.as_file().sync_all())
            .map_err(|e| failure(ErrorKind::Storage, e.to_string()))?;
        file.persist_noclobber(path).map_err(|e| {
            failure(
                ErrorKind::Storage,
                format!("write new kubeconfig {}: {e}", path.display()),
            )
        })?;
        Ok(())
    }

    /// Stop compute while keeping workloads, volumes, and credentials on disk.
    pub fn stop(&self) -> Result<()> {
        self.owned()?;
        self.machine.stop()
    }

    /// Permanently delete this managed cluster and its volumes.
    pub fn delete(&self) -> Result<()> {
        self.owned()?;
        self.machine.delete()
    }
}

fn dedicated_config(value: Value, name: &str, port: u16) -> Result<Value> {
    let cluster = &value["clusters"][0]["cluster"];
    let user = &value["users"][0]["user"];
    let string = |v: &Value| {
        v.as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                failure(
                    ErrorKind::Other,
                    "Cluster kubeconfig is missing certificate credentials",
                )
            })
    };
    let context = format!("smol-{name}");
    Ok(json!({"apiVersion":"v1","kind":"Config",
        "clusters":[{"name":context,"cluster":{"server":format!("https://127.0.0.1:{port}"),"certificate-authority-data":string(&cluster["certificate-authority-data"])?}}],
        "users":[{"name":context,"user":{"client-certificate-data":string(&user["client-certificate-data"])?,"client-key-data":string(&user["client-key-data"])?}}],
        "contexts":[{"name":context,"context":{"cluster":context,"user":context}}],"current-context":context}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_names_fail_before_machine_creation() {
        for name in [
            "",
            "../user",
            "$(touch file)",
            "foo/bar",
            "-foo",
            "Foo",
            "foo-",
        ] {
            assert!(KubernetesConfig::new(name).validate().is_err());
        }
        assert!(KubernetesConfig::new("work-42").validate().is_ok());
    }
    #[test]
    fn exported_config_cannot_inherit_exec_credentials_or_skip_tls() {
        let original = json!({"clusters":[{"cluster":{"server":"https://remote","certificate-authority-data":"ca","insecure-skip-tls-verify":true}}],"users":[{"user":{"client-certificate-data":"cert","client-key-data":"key","exec":{"command":"unexpected"}}}]});
        let exported = dedicated_config(original, "dev", 6444).unwrap();
        assert_eq!(
            exported["clusters"][0]["cluster"]["server"],
            "https://127.0.0.1:6444"
        );
        assert!(exported["users"][0]["user"].get("exec").is_none());
        assert!(exported["clusters"][0]["cluster"]
            .get("insecure-skip-tls-verify")
            .is_none());
        assert!(dedicated_config(json!({}), "dev", 6444).is_err());
    }
}
