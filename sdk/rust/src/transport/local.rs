//! The local engine, driven through an installed `smolvm` binary.
//!
//! The SDK deliberately does not link the engine crate. Linking it would make
//! this crate unpublishable — crates.io resolves every dependency, optional
//! ones included, and the engine is not published. Driving the CLI keeps the
//! SDK self-contained on crates.io while still running machines on this host.
//!
//! The cost is that a local machine needs `smolvm` installed; see
//! [`crate::RuntimeAssets::from_path_lookup`] for how it is found.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::{unsupported, ReadyOptions, Transport};
use crate::config::Port;
use crate::connect::Target;
use crate::error::{Error, ErrorKind, Result};
use crate::exec::{ExecEvent, ExecOptions, ExecResult, ExecStream};
use crate::machine::{
    BranchOptions, Checkpoint, CheckpointOptions, CheckpointResult, CloudCheckpoint, ImageInfo,
    MachineState, PortEndpoint, ShareLink, UsageReport,
};

/// How many branches boot at once when a batch does not say.
const DEFAULT_BRANCH_PARALLEL: usize = 8;

fn transfer_dir() -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("smolmachines-transfer-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder
        .tempdir()
        .map_err(|e| Error::new(ErrorKind::Storage, format!("stage file transfer: {e}")))
}

fn forward_output(
    mut reader: impl Read,
    tx: mpsc::Sender<ExecEvent>,
    event: fn(Vec<u8>) -> ExecEvent,
) {
    let mut buffer = [0; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                // Keep draining if the stream was dropped: the guest command
                // must not fail or block because its reader went away.
                let _ = tx.send(event(buffer[..n].to_vec()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let _ = tx.send(ExecEvent::Error(format!("read exec output: {e}")));
                break;
            }
        }
    }
}

/// Find the `smolvm` binary this transport drives.
///
/// `SMOLVM` wins so a caller can pin a specific build; otherwise the installed
/// CLI on `PATH`, then the two default install locations.
pub(crate) fn resolve_cli() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("SMOLVM").map(PathBuf::from) {
        return explicit_cli(explicit);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("smolvm");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for suffix in [".local/bin/smolvm", ".smolvm/smolvm"] {
        if let Some(candidate) = home.as_ref().map(|h| h.join(suffix)) {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    // Nothing installed: fetch the engine this SDK was built against. Only a
    // local machine ever gets here, and only once — it is cached afterwards.
    crate::bootstrap::ensure_engine()
}

fn explicit_cli(path: PathBuf) -> Result<PathBuf> {
    if path.is_file() {
        Ok(path)
    } else {
        Err(Error::new(
            ErrorKind::Config,
            format!("SMOLVM must name an existing binary: {}", path.display()),
        ))
    }
}

#[derive(Debug)]
pub(crate) struct LocalTransport {
    name: String,
    cli: PathBuf,
    /// Ports this SDK published for the machine. The CLI reports only a count,
    /// so a machine the SDK did not create cannot be asked for its mapping.
    ports: Vec<Port>,
}

impl LocalTransport {
    pub(crate) fn new(name: impl Into<String>) -> Result<Self> {
        Ok(Self {
            name: name.into(),
            cli: resolve_cli()?,
            ports: Vec::new(),
        })
    }

    pub(crate) fn with_ports(name: impl Into<String>, ports: Vec<Port>) -> Result<Self> {
        Ok(Self {
            ports,
            ..Self::new(name)?
        })
    }

    /// Run a CLI command and hand back its exit code and streams.
    fn cli(&self, args: &[&str]) -> Result<(i32, Vec<u8>, Vec<u8>)> {
        let output = Command::new(&self.cli)
            .args(args)
            .stdin(Stdio::null())
            // See assets.rs: this variable makes the engine tie the VM's life
            // to its parent, and every CLI call here is short-lived.
            .env_remove("SMOLVM_BOOT_BINARY")
            .output()
            .map_err(|e| {
                Error::new(ErrorKind::Other, format!("run {}: {e}", self.cli.display()))
            })?;
        Ok((
            output.status.code().unwrap_or(-1),
            output.stdout,
            output.stderr,
        ))
    }

    /// Run a CLI command that is expected to succeed.
    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let (code, stdout, stderr) = self.cli(args)?;
        if code == 0 {
            return Ok(stdout);
        }
        Err(cli_error(args, &stderr, &stdout))
    }

    /// This machine's row from `machine ls --json`.
    fn record(&self) -> Result<serde_json::Value> {
        let out = self.run(&["machine", "ls", "--json"])?;
        let rows: serde_json::Value = serde_json::from_slice(&out)
            .map_err(|e| Error::new(ErrorKind::Other, format!("read the machine list: {e}")))?;
        let rows = rows
            .as_array()
            .cloned()
            .or_else(|| rows.get("machines").and_then(|m| m.as_array()).cloned())
            .unwrap_or_default();
        rows.into_iter()
            .find(|row| row.get("name").and_then(|n| n.as_str()) == Some(&self.name))
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("VM not found: {}", self.name)))
    }

    fn exec_args<'a>(&'a self, command: &'a [String], options: &'a ExecOptions) -> Vec<String> {
        let mut args = vec![
            "machine".into(),
            "exec".into(),
            "--name".into(),
            self.name.clone(),
        ];
        for (key, value) in &options.env {
            args.push("--env".into());
            args.push(format!("{key}={value}"));
        }
        if let Some(workdir) = &options.workdir {
            args.push("--workdir".into());
            args.push(workdir.clone());
        }
        if let Some(timeout) = options.timeout {
            args.push("--timeout".into());
            args.push(format!("{}s", timeout.as_secs()));
        }
        args.push("--".into());
        args.extend(command.iter().cloned());
        args
    }
}

/// Turn a non-zero CLI exit into an error that keeps what the CLI said.
fn cli_error(args: &[&str], stderr: &[u8], stdout: &[u8]) -> Error {
    let message = String::from_utf8_lossy(if stderr.is_empty() { stdout } else { stderr })
        .trim()
        .to_string();
    let kind = if message.contains("not found") || message.contains("vm not found") {
        ErrorKind::NotFound
    } else if message.contains("already exists") || message.contains("in use") {
        ErrorKind::Conflict
    } else if message.contains("is frozen") || message.contains("not running") {
        ErrorKind::InvalidState
    } else {
        ErrorKind::Other
    };
    Error::new(
        kind,
        if message.is_empty() {
            format!("smolvm {} failed", args.join(" "))
        } else {
            message
        },
    )
}

impl Transport for LocalTransport {
    fn name(&self) -> &str {
        &self.name
    }

    fn id(&self) -> &str {
        &self.name
    }

    fn target(&self) -> Target {
        Target::Local
    }

    fn state(&self) -> MachineState {
        self.record()
            .ok()
            .and_then(|row| {
                row.get("state")
                    .and_then(|s| s.as_str())
                    .map(MachineState::parse)
            })
            .unwrap_or(MachineState::Unknown)
    }

    fn is_running(&self) -> bool {
        matches!(self.state(), MachineState::Running | MachineState::Started)
    }

    fn pid(&self) -> Option<i32> {
        self.record()
            .ok()
            .and_then(|row| row.get("pid").and_then(|p| p.as_i64()))
            .map(|pid| pid as i32)
    }

    fn ready(&self) -> Result<bool> {
        // A local machine's agent is connected as part of starting it, so
        // running is ready.
        Ok(self.is_running())
    }

    fn wait_until_ready(&self, options: ReadyOptions) -> Result<()> {
        let deadline = Instant::now() + options.timeout;
        loop {
            if self.ready()? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::new(
                    ErrorKind::Timeout,
                    format!(
                        "machine {} not ready after {:?} (state={})",
                        self.name,
                        options.timeout,
                        self.state()
                    ),
                ));
            }
            thread::sleep(options.interval);
        }
    }

    fn start(&self) -> Result<()> {
        self.run(&["machine", "start", "--name", &self.name])
            .map(|_| ())
    }

    fn start_branchable(&self) -> Result<()> {
        self.run(&["machine", "start", "--name", &self.name, "--branchable"])
            .map(|_| ())
    }

    fn stop(&self) -> Result<()> {
        self.run(&["machine", "stop", "--name", &self.name])
            .map(|_| ())
    }

    fn pause(&self) -> Result<()> {
        self.run(&["machine", "pause", "--name", &self.name])
            .map(|_| ())
    }

    fn resume(&self) -> Result<()> {
        self.run(&["machine", "resume", "--name", &self.name])
            .map(|_| ())
    }

    fn delete(&self) -> Result<()> {
        self.run(&["machine", "delete", "--name", &self.name, "--force"])
            .map(|_| ())
    }

    fn exec(&self, command: Vec<String>, options: ExecOptions) -> Result<ExecResult> {
        let args = self.exec_args(&command, &options);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let (exit_code, stdout, stderr) = self.cli(&borrowed)?;
        Ok(ExecResult {
            exit_code,
            stdout,
            stderr,
        })
    }

    fn exec_stream(&self, command: Vec<String>, options: ExecOptions) -> Result<ExecStream> {
        let mut args = self.exec_args(&command, &options);
        args.insert(2, "--stream".into());
        let mut child = Command::new(&self.cli)
            .args(&args)
            // Give the child no stdin. Inheriting the caller's makes the CLI
            // treat the exec as interactive and attach a terminal to the
            // guest, which is not what a streaming API asked for.
            .stdin(Stdio::null())
            .env_remove("SMOLVM_BOOT_BINARY")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::new(ErrorKind::Other, format!("start exec: {e}")))?;

        let (tx, rx) = mpsc::channel();
        let out = child.stdout.take().expect("piped stdout");
        let err = child.stderr.take().expect("piped stderr");
        let out_tx = tx.clone();
        let stdout = thread::spawn(move || forward_output(out, out_tx, ExecEvent::Stdout));
        let err_tx = tx.clone();
        let stderr = thread::spawn(move || forward_output(err, err_tx, ExecEvent::Stderr));
        thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
            let out_result = stdout.join();
            let err_result = stderr.join();
            if out_result.is_err() || err_result.is_err() {
                let _ = tx.send(ExecEvent::Error("exec output reader failed".into()));
            }
            let _ = tx.send(ExecEvent::Exit(code));
        });
        Ok(ExecStream::from_receiver(rx))
    }

    /// Run a command in a *fresh ephemeral* machine from `image`.
    ///
    /// The CLI's `run` always makes its own throwaway machine, so unlike the
    /// cloud target this does not execute inside this machine — it is the
    /// equivalent of `docker run`, not `docker exec`.
    fn run(&self, image: &str, command: Vec<String>, options: ExecOptions) -> Result<ExecResult> {
        let mut args: Vec<String> = vec![
            "machine".into(),
            "run".into(),
            "--image".into(),
            image.into(),
        ];
        for (key, value) in &options.env {
            args.push("--env".into());
            args.push(format!("{key}={value}"));
        }
        if let Some(workdir) = &options.workdir {
            args.push("--workdir".into());
            args.push(workdir.clone());
        }
        args.push("--".into());
        args.extend(command);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let (exit_code, stdout, stderr) = self.cli(&borrowed)?;
        Ok(ExecResult {
            exit_code,
            stdout,
            stderr,
        })
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        // `machine cp` writes to a host path, so stage through a temp file.
        let transfer = transfer_dir()?;
        let staged = transfer.path().join("payload");
        self.run(&[
            "machine",
            "cp",
            &format!("{}:{path}", self.name),
            &staged.to_string_lossy(),
        ])?;
        let data = std::fs::read(&staged)
            .map_err(|e| Error::new(ErrorKind::Storage, format!("read {path}: {e}")))?;
        Ok(data)
    }

    fn write_file(&self, path: &str, data: Vec<u8>, mode: Option<u32>) -> Result<()> {
        let transfer = transfer_dir()?;
        let staged = transfer.path().join("payload");
        std::fs::write(&staged, data)
            .map_err(|e| Error::new(ErrorKind::Storage, format!("stage {path}: {e}")))?;
        let staged_arg = staged.to_string_lossy().to_string();
        let target = format!("{}:{path}", self.name);
        let mode_arg = mode.map(|m| format!("{m:o}"));
        let mut args = vec!["machine", "cp", &staged_arg, &target];
        if let Some(mode) = &mode_arg {
            args.push("--mode");
            args.push(mode);
        }
        self.run(&args).map(|_| ())
    }

    fn pull_image(&self, _image: &str) -> Result<ImageInfo> {
        Err(unsupported(
            "pull_image()",
            "the engine fetches an image as part of creating a machine from it, and exposes no \
             way to pull into an existing machine's store — create the machine with that image, \
             or use run(), which pulls into an ephemeral one",
        ))
    }

    fn list_images(&self) -> Result<Vec<ImageInfo>> {
        let out = self.run(&["machine", "images", "--name", &self.name, "--json"])?;
        let parsed: serde_json::Value = serde_json::from_slice(&out)
            .map_err(|e| Error::new(ErrorKind::Other, format!("read the image list: {e}")))?;
        let rows = parsed
            .as_array()
            .cloned()
            .or_else(|| parsed.get("images").and_then(|i| i.as_array()).cloned())
            .unwrap_or_default();
        Ok(rows
            .into_iter()
            .map(|row| {
                let text = |key: &str| {
                    row.get(key)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string()
                };
                ImageInfo {
                    reference: text("reference"),
                    digest: text("digest"),
                    size: row.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
                    architecture: text("architecture"),
                    os: text("os"),
                }
            })
            .collect())
    }

    fn sync(&self) -> Result<()> {
        self.run(&["machine", "sync", "--name", &self.name])
            .map(|_| ())
    }

    fn host_port(&self, guest_port: u16) -> Result<Option<u16>> {
        // The CLI reports only how many ports a machine publishes, not the
        // mapping, so this answers from what the SDK itself asked for.
        Ok(self
            .ports
            .iter()
            .find(|port| port.guest == guest_port)
            .map(|port| port.host))
    }

    fn guest_ports(&self) -> Result<Vec<u16>> {
        Ok(self.ports.iter().map(|port| port.guest).collect())
    }

    fn endpoint(&self, port: u16, path: &str) -> Result<PortEndpoint> {
        let host_port = self.host_port(port)?.ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                format!("guest port {port} is not published by {}", self.name),
            )
        })?;
        let suffix = path.trim_start_matches('/');
        let rel = if suffix.is_empty() {
            String::new()
        } else {
            format!("/{suffix}")
        };
        Ok(PortEndpoint {
            http_url: format!("http://127.0.0.1:{host_port}{rel}"),
            ws_url: format!("ws://127.0.0.1:{host_port}{rel}"),
            headers: Vec::new(),
        })
    }

    fn tunnel_target(&self, port: u16) -> Result<crate::tunnel::Target> {
        let host = self
            .host_port(port)?
            .filter(|p| *p != 0)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "guest port is not published"))?;
        Ok(crate::tunnel::Target::Local(([127, 0, 0, 1], host).into()))
    }

    fn url(&self) -> Result<Option<String>> {
        Ok(self
            .ports
            .first()
            .map(|port| format!("http://127.0.0.1:{}", port.host)))
    }

    fn checkpoint(&self, output: Option<&Path>, options: CheckpointOptions) -> Result<Checkpoint> {
        let output = output.ok_or_else(|| {
            Error::new(
                ErrorKind::Config,
                "a local checkpoint writes to disk, so it needs an output path",
            )
        })?;
        let output_arg = output.to_string_lossy().to_string();
        let store_arg = options.store_dir.map(|d| d.to_string_lossy().to_string());
        let mut args = vec![
            "machine",
            "checkpoint",
            "--name",
            &self.name,
            "-o",
            &output_arg,
        ];
        if let Some(store) = &store_arg {
            args.push("--store");
            args.push(store);
        }
        let started = Instant::now();
        let out = self.run(&args)?;
        let summary = String::from_utf8_lossy(&out);
        // The CLI reports what the capture cost in prose. Reporting zeros
        // instead would look like a measurement rather than a missing one.
        let (size_bytes, source_pause) = parse_capture_summary(&summary);
        Ok(Checkpoint::Local(CheckpointResult {
            size_bytes: size_bytes
                .unwrap_or_else(|| std::fs::metadata(output).map(|m| m.len()).unwrap_or(0)),
            reused_bytes: parse_reused(&summary).unwrap_or(0),
            source_pause: source_pause.unwrap_or(Duration::ZERO),
            elapsed: started.elapsed(),
        }))
    }

    fn checkpoints(&self) -> Result<Vec<Checkpoint>> {
        Err(unsupported(
            "checkpoints()",
            "a local capture is a file you chose the path for, so the engine keeps no list; \
             this is a cloud target operation",
        ))
    }

    fn branch(&self, name: &str, options: &BranchOptions) -> Result<Box<dyn Transport>> {
        let mut args: Vec<String> = vec![
            "machine".into(),
            "branch".into(),
            "--from".into(),
            self.name.clone(),
            "--name".into(),
            name.into(),
        ];
        for port in &options.ports {
            args.push("-p".into());
            args.push(format!("{}:{}", port.host, port.guest));
        }
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&borrowed)?;
        Ok(Box::new(LocalTransport::with_ports(
            name,
            options.ports.clone(),
        )?))
    }

    fn branch_batch(
        &self,
        names: &[String],
        options: &BranchOptions,
    ) -> Result<Vec<Box<dyn Transport>>> {
        let _ = if options.parallel == 0 {
            DEFAULT_BRANCH_PARALLEL
        } else {
            options.parallel
        };
        // The CLI branches one child per call; a batch is those calls in order.
        let mut made: Vec<Box<dyn Transport>> = Vec::with_capacity(names.len());
        for name in names {
            match self.branch(name, options) {
                Ok(child) => made.push(child),
                Err(error) => {
                    // Transactional, like the engine's own batch: keep none.
                    for child in &made {
                        let _ = child.delete();
                    }
                    return Err(error);
                }
            }
        }
        Ok(made)
    }

    fn usage(&self) -> Result<UsageReport> {
        Err(unsupported(
            "usage()",
            "nothing meters a machine running on your own hardware; this is a cloud target \
             operation",
        ))
    }

    fn delete_with_usage(&self) -> Result<UsageReport> {
        Err(unsupported(
            "delete_with_usage()",
            "nothing meters a machine running on your own hardware; this is a cloud target \
             operation",
        ))
    }

    fn share(&self) -> Result<ShareLink> {
        Err(unsupported(
            "share()",
            "a local machine has no public ingress to share; this is a cloud target operation",
        ))
    }

    fn unshare(&self) -> Result<()> {
        Err(unsupported(
            "unshare()",
            "a local machine has no public ingress to share; this is a cloud target operation",
        ))
    }
}

/// Create a machine on this host and return a handle to it.
pub(crate) fn create(
    cli: &Path,
    args: Vec<String>,
    name: &str,
    ports: Vec<Port>,
) -> Result<LocalTransport> {
    let output = Command::new(cli)
        .args(&args)
        .stdin(Stdio::null())
        .env_remove("SMOLVM_BOOT_BINARY")
        .output()
        .map_err(|e| Error::new(ErrorKind::Other, format!("run {}: {e}", cli.display())))?;
    if !output.status.success() {
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        return Err(cli_error(&borrowed, &output.stderr, &output.stdout));
    }
    LocalTransport::with_ports(name, ports)
}

/// Unused on this transport, kept so the cloud checkpoint type stays shared.
#[allow(dead_code)]
fn _cloud_checkpoint_is_shared(_: CloudCheckpoint) {}

/// Read `(583 MiB written, 5.931s total, 0.521s source pause)` off the CLI's
/// own summary line.
fn parse_capture_summary(text: &str) -> (Option<u64>, Option<Duration>) {
    let size = text
        .split_once(" MiB written")
        .and_then(|(head, _)| head.rsplit(['(', ' ']).next().map(str::to_string))
        .and_then(|n| n.parse::<f64>().ok())
        .map(|mib| (mib * 1024.0 * 1024.0) as u64);
    let pause = text
        .split_once("s source pause")
        .and_then(|(head, _)| head.rsplit([',', ' ']).next().map(str::to_string))
        .and_then(|n| n.parse::<f64>().ok())
        .map(Duration::from_secs_f64);
    (size, pause)
}

/// Read `Reused 1702 MiB from existing checkpoint objects`.
fn parse_reused(text: &str) -> Option<u64> {
    let after = text.split_once("Reused ")?.1;
    let mib: f64 = after.split_whitespace().next()?.parse().ok()?;
    Some((mib * 1024.0 * 1024.0) as u64)
}

#[cfg(test)]
mod capture_summary_tests {
    use super::*;

    #[test]
    fn the_cli_summary_is_read_back_as_numbers() {
        let text = "Checkpointed 'x' to /p (583 MiB written, 5.931s total, 0.521s source pause)\n\
                    Reused 1702 MiB from existing checkpoint objects\n";
        let (size, pause) = parse_capture_summary(text);
        assert_eq!(size, Some(583 * 1024 * 1024));
        assert_eq!(pause, Some(Duration::from_secs_f64(0.521)));
        assert_eq!(parse_reused(text), Some(1702 * 1024 * 1024));
    }

    #[test]
    fn a_summary_without_figures_reports_nothing_rather_than_zero() {
        let (size, pause) = parse_capture_summary("Checkpointed 'x' to /p\n");
        assert!(size.is_none() && pause.is_none());
        assert!(parse_reused("nothing here").is_none());
    }
}

#[cfg(all(test, unix))]
mod io_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn cli(root: &Path, script: &str) -> LocalTransport {
        let path = root.join("cli");
        std::fs::write(&path, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        LocalTransport {
            name: "test".into(),
            cli: path,
            ports: vec![],
        }
    }

    #[test]
    fn streaming_preserves_bytes_before_exit_and_delivers_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let transport = cli(dir.path(), "test \"$3\" = --stream\nprintf 'first\\000'\nsleep 1\nprintf '\\nlast\\n'\nprintf 'error\\n' >&2\nexit 7");
        let start = Instant::now();
        let mut stream = transport
            .exec_stream(vec!["true".into()], ExecOptions::new())
            .unwrap();
        assert_eq!(stream.next(), Some(ExecEvent::Stdout(b"first\0".to_vec())));
        assert!(start.elapsed() < Duration::from_millis(800));
        let events: Vec<_> = stream.collect();
        assert_eq!(events.last(), Some(&ExecEvent::Exit(7)));
        let mut out = Vec::new();
        let mut err = Vec::new();
        for event in events {
            match event {
                ExecEvent::Stdout(b) => out.extend(b),
                ExecEvent::Stderr(b) => err.extend(b),
                ExecEvent::Exit(_) => (),
                ExecEvent::Error(e) => panic!("{e}"),
            }
        }
        assert_eq!(out, b"\nlast\n");
        assert_eq!(err, b"error\n");
    }

    #[test]
    fn transfers_use_private_independent_staging_and_clean_up_on_failure() {
        let a = transfer_dir().unwrap();
        let b = transfer_dir().unwrap();
        assert_ne!(a.path(), b.path());
        assert_eq!(
            std::fs::metadata(a.path()).unwrap().permissions().mode() & 0o077,
            0
        );
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("staged");
        let transport = cli(
            dir.path(),
            &format!("printf '%s' \"$3\" > '{}'\nexit 1", record.display()),
        );
        assert!(transport
            .write_file("/same", b"data".to_vec(), None)
            .is_err());
        let staged = PathBuf::from(std::fs::read_to_string(&record).unwrap());
        assert!(!staged.parent().unwrap().exists());
        let transport = cli(
            dir.path(),
            &format!("printf '%s' \"$4\" > '{}'\nexit 1", record.display()),
        );
        assert!(transport.read_file("/same").is_err());
        let staged = PathBuf::from(std::fs::read_to_string(record).unwrap());
        assert!(!staged.parent().unwrap().exists());
    }

    #[test]
    fn explicit_runtime_must_exist_and_be_a_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(explicit_cli(dir.path().join("missing")).is_err());
        assert!(explicit_cli(dir.path().into()).is_err());
        let file = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        assert_eq!(explicit_cli(file.path().into()).unwrap(), file.path());
    }

    #[test]
    fn output_errors_are_not_overwritten_by_a_successful_exit() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("broken pipe"))
            }
        }
        let (tx, rx) = mpsc::channel();
        forward_output(Broken, tx.clone(), ExecEvent::Stdout);
        tx.send(ExecEvent::Exit(0)).unwrap();
        drop(tx);
        let result = ExecStream::from_receiver(rx).collect_result();
        assert_eq!(result.exit_code, -1);
        assert!(result.stderr_utf8().contains("broken pipe"));
    }
}
