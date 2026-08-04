//! smol docker — run a real Docker engine in a smol microVM and expose it as a
//! host Docker endpoint.
//!
//! Point your existing Docker tooling at it and everything works unchanged:
//!
//! ```text
//! eval "$(smol docker)"          # or: export DOCKER_HOST=tcp://127.0.0.1:2375
//! docker build .                 # real dockerd, real buildkit
//! docker compose up              # real compose, not a partial reimplementation
//! docker buildx build ...        # multi-arch works
//! ```
//!
//! Unlike Docker Desktop / Podman on macOS, the daemon runs on smol's real Linux
//! kernel, so nested containers, k3d, and buildx behave like they do on Linux —
//! and it starts in well under a second.
//!
//! v1 scope: the Docker API (build, run, compose, buildx, testcontainers, CI).
//! Host bind mounts (`docker run -v /host:/ctr`) are a known follow-up — for
//! live-code development use `smol run -v ./src:/app` directly, which also gives
//! host→guest hot-reload.

use clap::Args;
use smolvm::agent::{AgentClient, AgentManager, LaunchFeatures, RunConfig, VmResources};
use smolvm::data::network::PortMapping;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the SIGINT/SIGTERM handler so the main loop can shut the VM down
/// cleanly instead of leaking it.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

#[derive(Args, Debug)]
pub struct DockerCmd {
    /// Host port to expose the Docker API on (DOCKER_HOST=tcp://127.0.0.1:PORT).
    #[arg(long, default_value_t = 2375)]
    pub port: u16,

    /// Docker-in-Docker image providing the engine.
    #[arg(long, default_value = "docker:dind")]
    pub image: String,

    /// Number of vCPUs for the engine VM.
    #[arg(long, value_name = "N")]
    pub cpus: Option<u8>,

    /// Memory in MiB for the engine VM.
    #[arg(long, value_name = "MiB")]
    pub mem: Option<u32>,

    /// Print only the export line (for `eval "$(smol docker --quiet)"`) and keep
    /// running silently.
    #[arg(short = 'q', long)]
    pub quiet: bool,
}

impl DockerCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let ports = vec![PortMapping::parse(&format!("{}:2375", self.port))
            .map_err(|e| anyhow::anyhow!(e))?];

        let resources = VmResources {
            cpus: self.cpus.unwrap_or(4),
            memory_mib: self.mem.unwrap_or(8192),
            network: true,
            storage_gib: None,
            overlay_gib: None,
            allowed_cidrs: None,
            network_backend: None,
            gpu: false,
            gpu_vram_mib: None,
            dns: None,
        };

        // A registry reference is pulled in-guest; a local archive/dir is staged
        // and mounted. Mirrors `smol run`'s image handling.
        use smolvm::data::image_source::{classify, resolve, ResolvedImage};
        let reference = match resolve(classify(&self.image))? {
            ResolvedImage::Registry(r) => r,
            ResolvedImage::Local { reference, .. } => reference,
        };

        let manager = AgentManager::new_default()?;
        if !self.quiet {
            eprintln!("Starting Docker engine in a smol microVM...");
        }
        let features = LaunchFeatures::default();
        manager.ensure_running_with_full_config(vec![], ports, resources, features)?;
        let mut client = AgentClient::connect_with_retry(manager.vsock_socket())?;

        if !self.quiet {
            eprintln!("Pulling {}...", self.image);
        }
        client.pull_with_registry_config(&reference)?;

        // dockerd listens on the guest's tcp:2375; the published port forwards it
        // to the host loopback. overlay2 on the per-VM /storage disk gives a real
        // writable graph driver.
        let dockerd = "dockerd --host=tcp://0.0.0.0:2375 --tls=false \
                       --data-root=/storage/docker --storage-driver=overlay2";
        // A persistent overlay keeps the engine's images/containers across
        // `smol docker` restarts and is required for a detached/background run.
        let config = RunConfig::new(reference, vec!["sh".into(), "-c".into(), dockerd.into()])
            .with_persistent_overlay(Some("smol-docker".into()));
        client.run_background(config)?;

        wait_ready(self.port)?;

        // Clean shutdown on Ctrl-C / termination.
        unsafe {
            let handler = on_signal as *const () as libc::sighandler_t;
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
        }

        // stdout: the machine-readable export line (so `eval "$(smol docker)"`
        // works). stderr: the human guidance.
        println!("export DOCKER_HOST=tcp://127.0.0.1:{}", self.port);
        if !self.quiet {
            eprintln!(
                "\nDocker engine ready on smol's real Linux kernel.\n  \
                 export DOCKER_HOST=tcp://127.0.0.1:{port}\n\
                 Now `docker`, `docker compose`, `docker buildx`, and testcontainers all work.\n\
                 Press Ctrl-C to stop.",
                port = self.port
            );
        }

        while !STOP.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        if !self.quiet {
            eprintln!("\nStopping Docker engine...");
        }
        manager.kill();
        Ok(())
    }
}

/// Poll the forwarded Docker API until `/_ping` answers 200, or time out.
fn wait_ready(port: u16) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if STOP.load(Ordering::SeqCst) {
            anyhow::bail!("interrupted before the Docker engine was ready");
        }
        if let Ok(mut sock) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
            if sock
                .write_all(b"GET /_ping HTTP/1.0\r\nHost: localhost\r\n\r\n")
                .is_ok()
            {
                let mut buf = Vec::new();
                let _ = sock.take(256).read_to_end(&mut buf);
                // dockerd answers `HTTP/1.1 200 OK` with body `OK`.
                if buf.windows(3).any(|w| w == b"200") {
                    return Ok(());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    anyhow::bail!("the Docker engine did not become ready within 90s")
}
