//! smol compose — run an unmodified `docker-compose.yml` against a real Docker
//! engine running inside a smol microVM.
//!
//! Unlike `podman compose` / quadlets, this is not a re-implementation of
//! Compose: it boots `dockerd` inside a per-project microVM and runs the actual
//! `docker compose` CLI, so behavior is byte-for-byte Docker. The project
//! directory is mounted writable at `/workspace`, ports published by the compose
//! file are auto-forwarded to the host, and `dockerd` persists across
//! invocations (via the machine's keep-alive container) so `up -d`, `ps`,
//! `logs`, and `down` all talk to the same daemon.

use std::path::{Path, PathBuf};

use clap::Parser;
use smolvm::agent::{AgentManager, ExecEvent, LaunchFeatures, RunConfig, VmResources};
use smolvm::config::{SmolvmConfig, VmRecord};
use smolvm::data::network::PortMapping;
use smolvm::data::storage::HostMount;

/// Docker-in-Docker image: ships `dockerd`, the `docker` CLI, and the Compose
/// plugin. Overridable with `--image` for pinning or a custom base.
const DIND_IMAGE: &str = "docker:dind";
/// Where the project directory is mounted inside the VM.
const WORKSPACE: &str = "/workspace";

#[derive(Parser, Debug)]
pub struct ComposeCmd {
    /// Compose file to use. Defaults to auto-detecting docker-compose.yml /
    /// compose.yaml in the current directory.
    #[arg(short = 'f', long = "file")]
    file: Option<PathBuf>,

    /// Additional host port to forward (HOST:VM), on top of the ports the
    /// compose file already publishes. Repeatable.
    #[arg(short = 'p', long = "port", value_parser = PortMapping::parse, value_name = "HOST:VM")]
    port: Vec<PortMapping>,

    /// vCPUs for the microVM.
    #[arg(long, default_value_t = 4)]
    cpus: u8,

    /// Memory (MiB) for the microVM.
    #[arg(long = "mem", default_value_t = 8192)]
    mem: u32,

    /// Docker-capable image to run the engine inside (needs dockerd + compose).
    #[arg(long, default_value = DIND_IMAGE)]
    image: String,

    /// Custom DNS resolver for the microVM (e.g. 1.1.1.1). Useful when the host
    /// resolver is unreachable. Defaults to the backend's resolver.
    #[arg(long, value_name = "IP")]
    dns: Option<std::net::Ipv4Addr>,

    /// Arguments passed straight through to `docker compose` — e.g. `up`,
    /// `up -d`, `down`, `ps`, `logs -f web`, `build`. Defaults to `up`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

impl ComposeCmd {
    pub fn run(self) -> anyhow::Result<()> {
        // 1. Locate the compose file and the project directory.
        let compose_path = resolve_compose_file(self.file.as_deref())?;
        let project_dir = compose_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()?;
        let compose_file_name = compose_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "docker-compose.yml".into());

        // 2. Discover the ports the compose file publishes, plus any -p overrides.
        let mut ports = parse_compose_ports(&compose_path);
        for p in &self.port {
            if !ports.iter().any(|e| e.host == p.host) {
                ports.push(p.clone());
            }
        }

        // 3. One microVM per project directory, reused across invocations.
        let name = compose_machine_name(&project_dir);
        let storage = Some(20u64);
        let overlay = Some(30u64); // Docker layers/images need headroom.

        // 4. Mount the project dir writable at /workspace.
        let mount = HostMount::new(project_dir.as_path(), WORKSPACE, false)?;
        let mounts = vec![mount];

        // 5. Create the machine record on first use; reuse it afterwards.
        let mut config = SmolvmConfig::load()?;
        if config.get_vm(&name).is_none() {
            let mounts_tuples: Vec<(String, String, bool)> = mounts
                .iter()
                .map(|m| {
                    (
                        m.source.to_string_lossy().into_owned(),
                        m.target.to_string_lossy().into_owned(),
                        m.read_only,
                    )
                })
                .collect();
            let ports_tuples = PortMapping::to_tuples(&ports);
            let mut record = VmRecord::new(
                name.clone(),
                self.cpus,
                self.mem,
                mounts_tuples,
                ports_tuples,
                true,
            );
            record.image = Some(self.image.clone());
            record.storage_gb = storage;
            record.overlay_gb = overlay;
            record.workdir = Some(WORKSPACE.to_string());
            record.dns = self.dns;
            config.insert_vm(name.clone(), record)?;
        }

        // 6. Boot (or reconnect to) the microVM with the mount + ports.
        let manager = AgentManager::for_vm_with_sizes(&name, storage, overlay)?;
        let resources = VmResources {
            cpus: self.cpus,
            memory_mib: self.mem,
            network: true,
            storage_gib: storage,
            overlay_gib: overlay,
            allowed_cidrs: None,
            network_backend: None,
            gpu: false,
            gpu_vram_mib: None,
            dns: self.dns,
        };
        if !ports.is_empty() {
            let list: Vec<String> = ports
                .iter()
                .map(|p| format!("{}→{}", p.host, p.guest))
                .collect();
            eprintln!("smol: forwarding ports {}", list.join(", "));
        }
        eprintln!("smol: starting Docker microVM '{}' ({})", name, self.image);
        manager.ensure_running_with_full_config(
            mounts,
            ports,
            resources,
            LaunchFeatures::default(),
        )?;

        // 7. Assemble the in-VM command: ensure dockerd, then run docker compose.
        let compose_args = if self.args.is_empty() {
            vec!["up".to_string()]
        } else {
            self.args.clone()
        };
        let script = build_in_vm_script(&compose_file_name, &compose_args);

        // 8. Stream it inside the machine's persistent container overlay so the
        //    daemon, images, and volumes all survive across invocations.
        let mut client = smolvm::AgentClient::connect_with_retry(manager.vsock_socket())?;
        let cfg = RunConfig::new(&self.image, vec!["sh".into(), "-c".into(), script])
            .with_workdir(Some(WORKSPACE.to_string()))
            .with_persistent_overlay(Some(name.clone()));

        let mut exit_code = 0;
        client.run_streaming_with(cfg, |event| match event {
            ExecEvent::Stdout(data) => {
                use std::io::Write;
                let _ = std::io::stdout().write_all(&data);
                let _ = std::io::stdout().flush();
            }
            ExecEvent::Stderr(data) => {
                use std::io::Write;
                let _ = std::io::stderr().write_all(&data);
                let _ = std::io::stderr().flush();
            }
            ExecEvent::Exit(code) => exit_code = code,
            ExecEvent::Error(msg) => {
                eprintln!("smol: {}", msg);
                exit_code = 1;
            }
        })?;

        // Leave the VM running so the next `smol compose ...` reuses it.
        manager.detach();
        std::process::exit(exit_code);
    }
}

/// The shell run inside the VM: start dockerd if it isn't already reachable
/// (backgrounded so it persists in the keep-alive container), wait for the
/// socket, then exec the real `docker compose`.
fn build_in_vm_script(compose_file: &str, args: &[String]) -> String {
    let quoted_args = args
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"set -e
if ! docker info >/dev/null 2>&1; then
  echo "smol: starting Docker daemon..." >&2
  DR=/var/lib/docker
  if mkdir -p /storage/docker 2>/dev/null; then DR=/storage/docker; fi
  dockerd --data-root="$DR" --storage-driver=overlay2 >/var/log/smol-dockerd.log 2>&1 &
  for i in $(seq 1 60); do docker info >/dev/null 2>&1 && break; sleep 0.5; done
  if ! docker info >/dev/null 2>&1; then
    echo "smol: Docker daemon failed to start:" >&2
    tail -n 30 /var/log/smol-dockerd.log >&2 || true
    exit 1
  fi
  echo "smol: Docker daemon ready (data-root=$DR)" >&2
fi
exec docker compose -f {file} {args}
"#,
        file = shell_quote(compose_file),
        args = quoted_args,
    )
}

/// Minimal single-quote shell escaping.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Find the compose file: an explicit `-f`, else the usual names in cwd.
fn resolve_compose_file(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        anyhow::bail!("compose file not found: {}", p.display());
    }
    for cand in [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Ok(p);
        }
    }
    anyhow::bail!(
        "no compose file found (looked for docker-compose.yml / compose.yaml). \
         Pass one with -f <file>."
    )
}

/// Parse the host ports the compose file publishes so we can forward them.
/// Best-effort: unparseable files just yield no auto-forwards (users can still
/// use -p). Handles short syntax (`host:container`, `ip:host:container`,
/// `container` — the last has no host port) and long syntax (`published:`).
fn parse_compose_ports(path: &Path) -> Vec<PortMapping> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
        return Vec::new();
    };
    let mut host_ports: Vec<u16> = Vec::new();
    if let Some(services) = doc.get("services").and_then(|v| v.as_mapping()) {
        for (_svc, def) in services {
            if let Some(list) = def.get("ports").and_then(|v| v.as_sequence()) {
                for entry in list {
                    if let Some(hp) = extract_host_port(entry) {
                        host_ports.push(hp);
                    }
                }
            }
        }
    }
    host_ports.sort_unstable();
    host_ports.dedup();
    host_ports
        .into_iter()
        .filter_map(|hp| PortMapping::parse(&format!("{hp}:{hp}")).ok())
        .collect()
}

fn extract_host_port(entry: &serde_yaml::Value) -> Option<u16> {
    match entry {
        serde_yaml::Value::String(s) => parse_short_port(s),
        // A bare number is a container port only (no host port).
        serde_yaml::Value::Number(_) => None,
        serde_yaml::Value::Mapping(_) => entry.get("published").and_then(|v| {
            v.as_u64()
                .map(|n| n as u16)
                .or_else(|| v.as_str().and_then(|s| s.split('/').next()?.parse().ok()))
        }),
        _ => None,
    }
}

fn parse_short_port(s: &str) -> Option<u16> {
    let s = s.split('/').next().unwrap_or(s); // strip /tcp|/udp
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [_container] => None, // "80" — container-only, host picks a port
        [host, _container] => host.parse().ok(), // "8080:80"
        [_ip, host, _container] => host.parse().ok(), // "127.0.0.1:8080:80"
        _ => None,
    }
}

/// A stable, filesystem-safe machine name for a project directory.
fn compose_machine_name(dir: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let base = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into());
    let sanitized: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    dir.hash(&mut hasher);
    let short = hasher.finish() & 0xffff;
    format!("compose-{}-{:04x}", sanitized.trim_matches('-'), short)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_port_syntaxes() {
        assert_eq!(parse_short_port("8080:80"), Some(8080));
        assert_eq!(parse_short_port("127.0.0.1:8080:80"), Some(8080));
        assert_eq!(parse_short_port("8080:80/tcp"), Some(8080));
        assert_eq!(parse_short_port("80"), None);
    }

    #[test]
    fn parses_ports_from_compose() {
        let dir = std::env::temp_dir().join(format!("smol-compose-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("docker-compose.yml");
        std::fs::write(
            &f,
            "services:\n  web:\n    image: nginx\n    ports:\n      - \"8080:80\"\n      - \"127.0.0.1:5432:5432\"\n  db:\n    image: postgres\n    ports:\n      - 6379\n",
        )
        .unwrap();
        let ports = parse_compose_ports(&f);
        let hosts: Vec<u16> = ports.iter().map(|p| p.host).collect();
        assert!(hosts.contains(&8080));
        assert!(hosts.contains(&5432));
        assert!(!hosts.contains(&6379)); // container-only, no host port
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn machine_name_is_stable_and_safe() {
        let d = Path::new("/tmp/My Project!");
        let n = compose_machine_name(d);
        assert!(n.starts_with("compose-my-project-"));
        assert_eq!(n, compose_machine_name(d));
    }
}
