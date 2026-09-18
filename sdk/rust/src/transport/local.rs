//! The embedded engine, running the machine in this process.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use smolvm::embedded::{runtime, EmbeddedRuntime};

use super::{unsupported, ReadyOptions, Transport};
use crate::connect::Target;
use crate::error::{Error, ErrorKind, Result};
use crate::exec::{ExecOptions, ExecResult, ExecStream};
use crate::machine::{
    BranchOptions, Checkpoint, CheckpointOptions, ImageInfo, MachineState, PortEndpoint, ShareLink,
    UsageReport,
};

/// How many branches boot at once when a batch does not say.
const DEFAULT_BRANCH_PARALLEL: usize = 8;

#[derive(Debug)]
pub(crate) struct LocalTransport {
    name: String,
}

impl LocalTransport {
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    fn runtime(&self) -> Result<Arc<EmbeddedRuntime>> {
        Ok(runtime()?)
    }
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
        runtime()
            .map(|runtime| MachineState::parse(&runtime.state(&self.name)))
            .unwrap_or(MachineState::Stopped)
    }

    fn is_running(&self) -> bool {
        runtime()
            .map(|runtime| runtime.is_running(&self.name))
            .unwrap_or(false)
    }

    fn pid(&self) -> Option<i32> {
        runtime().ok().and_then(|runtime| runtime.pid(&self.name))
    }

    fn ready(&self) -> Result<bool> {
        // The embedded engine connects the agent as part of starting, so a
        // running local machine is by construction ready for work.
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
            std::thread::sleep(options.interval);
        }
    }

    fn start(&self) -> Result<()> {
        Ok(self.runtime()?.start_machine(&self.name)?)
    }

    fn start_branchable(&self) -> Result<()> {
        Ok(self.runtime()?.start_forkable_machine(&self.name)?)
    }

    fn stop(&self) -> Result<()> {
        Ok(self.runtime()?.stop_machine(&self.name)?)
    }

    fn delete(&self) -> Result<()> {
        Ok(self.runtime()?.delete_machine(&self.name)?)
    }

    fn exec(&self, command: Vec<String>, options: ExecOptions) -> Result<ExecResult> {
        let (env, workdir, timeout) = options.split();
        let (exit_code, stdout, stderr) = self
            .runtime()?
            .exec(&self.name, command, env, workdir, timeout)?;
        Ok(ExecResult {
            exit_code,
            stdout,
            stderr,
        })
    }

    fn exec_stream(&self, command: Vec<String>, options: ExecOptions) -> Result<ExecStream> {
        let (env, workdir, timeout) = options.split();
        Ok(ExecStream::spawn_local(
            self.name.clone(),
            command,
            env,
            workdir,
            timeout,
        ))
    }

    fn run(&self, image: &str, command: Vec<String>, options: ExecOptions) -> Result<ExecResult> {
        let (env, workdir, timeout) = options.split();
        let (exit_code, stdout, stderr) = self
            .runtime()?
            .run(&self.name, image, command, env, workdir, timeout)?;
        Ok(ExecResult {
            exit_code,
            stdout,
            stderr,
        })
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        Ok(self.runtime()?.read_file(&self.name, path)?)
    }

    fn write_file(&self, path: &str, data: Vec<u8>, mode: Option<u32>) -> Result<()> {
        Ok(self.runtime()?.write_file(&self.name, path, data, mode)?)
    }

    fn pull_image(&self, image: &str) -> Result<ImageInfo> {
        Ok(self.runtime()?.pull_image(&self.name, image)?.into())
    }

    fn list_images(&self) -> Result<Vec<ImageInfo>> {
        Ok(self
            .runtime()?
            .list_images(&self.name)?
            .into_iter()
            .map(ImageInfo::from)
            .collect())
    }

    fn sync(&self) -> Result<()> {
        Ok(self.runtime()?.sync_machine(&self.name)?)
    }

    fn host_port(&self, guest_port: u16) -> Result<Option<u16>> {
        Ok(self.runtime()?.host_port(&self.name, guest_port)?)
    }

    fn guest_ports(&self) -> Result<Vec<u16>> {
        Ok(self.runtime()?.guest_ports(&self.name)?)
    }

    fn endpoint(&self, port: u16, path: &str) -> Result<PortEndpoint> {
        // A local published port is a plain localhost host-port mapping, with
        // no bridge and nothing to authenticate against.
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

    fn url(&self) -> Result<Option<String>> {
        // Local machines have no ingress of their own. The first published port
        // on loopback is the closest honest answer.
        let Some(&port) = self.guest_ports()?.first() else {
            return Ok(None);
        };
        Ok(self
            .host_port(port)?
            .map(|host_port| format!("http://127.0.0.1:{host_port}")))
    }

    fn checkpoint(&self, output: Option<&Path>, options: CheckpointOptions) -> Result<Checkpoint> {
        let output = output.ok_or_else(|| {
            Error::new(
                ErrorKind::Config,
                "a local checkpoint writes to disk, so it needs an output path",
            )
        })?;
        let capture = smolvm::portable_checkpoint::CaptureOptions {
            rootfs_dir: Some(smolvm::agent::AgentManager::default_rootfs_path()?),
            store_dir: options.store_dir,
            staging_dir: options.staging_dir,
            prepared_cache_budget_bytes: options.prepared_cache_budget_bytes,
            ..Default::default()
        };
        Ok(Checkpoint::Local(
            self.runtime()?
                .checkpoint_machine(&self.name, output, &capture)?
                .into(),
        ))
    }

    fn checkpoints(&self) -> Result<Vec<Checkpoint>> {
        Err(unsupported(
            "checkpoints()",
            "a local checkpoint is a file you chose the path for, so the engine keeps no list; \
             this is a cloud target operation",
        ))
    }

    fn branch(&self, name: &str, options: &BranchOptions) -> Result<Box<dyn Transport>> {
        let pinned = options.pinned();
        let runtime = self.runtime()?;
        if options.checkpointable {
            runtime.fork_checkpointable_machine(&self.name, name, &pinned)?;
        } else {
            runtime.fork_machine(&self.name, name, &pinned)?;
        }
        Ok(Box::new(LocalTransport::new(name)))
    }

    fn branch_batch(
        &self,
        names: &[String],
        options: &BranchOptions,
    ) -> Result<Vec<Box<dyn Transport>>> {
        let parallel = if options.parallel == 0 {
            DEFAULT_BRANCH_PARALLEL
        } else {
            options.parallel
        };
        self.runtime()?
            .fork_machines(&self.name, names, &options.pinned(), parallel)?;
        Ok(names
            .iter()
            .map(|name| Box::new(LocalTransport::new(name)) as Box<dyn Transport>)
            .collect())
    }

    fn usage(&self) -> Result<UsageReport> {
        Err(unsupported(
            "usage()",
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
