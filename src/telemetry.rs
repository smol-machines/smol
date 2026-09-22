//! Anonymous usage telemetry.
//!
//! Each command records one event — which command ran (the subcommand path,
//! never its arguments), whether it succeeded, how long it took, and the CLI
//! version and platform — into a local spool. The next invocation uploads the
//! spool in a background thread, so no command ever waits on the network. When
//! the CLI is logged in, the upload carries the session's API key and the
//! control plane attributes the events to that tenant; otherwise they are only
//! tied to a random per-installation id.
//!
//! Off switches, any of which wins: `SMOL_TELEMETRY=0`, `DO_NOT_TRACK=1`, or
//! `smol config set telemetry off`.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Environment variable that disables telemetry when set to `0`, `off` or `false`.
pub const CONSENT_ENV: &str = "SMOL_TELEMETRY";
/// The cross-tool opt-out convention (<https://consoledonottrack.com>).
const DO_NOT_TRACK_ENV: &str = "DO_NOT_TRACK";
/// Events kept locally while uploads fail; older ones are dropped first.
const MAX_PENDING: usize = 500;
/// Events per upload.
const BATCH: usize = 100;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(3);

/// What the user has chosen, and where the choice came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub enabled: bool,
    /// `environment`, `config`, or `default`.
    pub source: &'static str,
    pub install_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    #[serde(default = "yes")]
    enabled: bool,
    install_id: String,
}

fn yes() -> bool {
    true
}

/// One recorded event, in the control plane's wire shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    id: String,
    sent_at: String,
    install_id: String,
    cli_version: String,
    os: String,
    arch: String,
    command: String,
    ok: bool,
    duration_ms: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Batch<'a> {
    events: &'a [Event],
}

/// An in-flight command, handed back to [`finish`].
pub struct Session {
    command: String,
    started: Instant,
    settings: Settings,
}

fn config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".config").join("smolvm"))
}

fn settings_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("telemetry.toml"))
}

fn spool_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("telemetry"))
}

fn env_disabled() -> bool {
    let off = |value: String| {
        let v = value.trim().to_ascii_lowercase();
        v == "0" || v == "off" || v == "false" || v == "no"
    };
    std::env::var(CONSENT_ENV).ok().is_some_and(off)
        || std::env::var(DO_NOT_TRACK_ENV)
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0")
}

fn load_settings() -> Option<Settings> {
    let path = settings_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&text).ok()
}

fn save_settings(settings: &Settings) -> std::io::Result<()> {
    let path = settings_path().ok_or_else(|| std::io::Error::other("no home directory"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string(settings).map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}

fn new_install_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("operating system randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Load settings, creating them (and printing the one-time notice) on first use.
fn settings_or_init() -> Option<Settings> {
    if let Some(settings) = load_settings() {
        return Some(settings);
    }
    let settings = Settings {
        enabled: true,
        install_id: new_install_id(),
    };
    if save_settings(&settings).is_err() {
        return None;
    }
    eprintln!(
        "smol collects anonymous usage data (command names, version, OS, success and \
         duration — never arguments, file names or output) to guide development.\n\
         Turn it off any time: smol config set telemetry off"
    );
    Some(settings)
}

/// The effective setting.
pub fn status() -> Status {
    let settings = load_settings();
    let install_id = settings.as_ref().map(|s| s.install_id.clone());
    if env_disabled() {
        return Status {
            enabled: false,
            source: "environment",
            install_id,
        };
    }
    match settings {
        Some(s) => Status {
            enabled: s.enabled,
            source: "config",
            install_id,
        },
        None => Status {
            enabled: true,
            source: "default",
            install_id,
        },
    }
}

/// Persist the user's choice. Disabling also discards anything not yet sent.
pub fn set_enabled(enabled: bool) -> std::io::Result<()> {
    let mut settings = load_settings().unwrap_or_else(|| Settings {
        enabled,
        install_id: new_install_id(),
    });
    settings.enabled = enabled;
    save_settings(&settings)?;
    if !enabled {
        if let Some(dir) = spool_dir() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    Ok(())
}

/// The recordable command path for an argument vector: the top-level command
/// plus, for commands that have subcommands, the subcommand — and nothing else.
/// Anything that is not a lowercase word is dropped, so no user value can slip
/// through as a "command".
pub fn command_label(args: &[String]) -> Option<String> {
    const WITH_SUBCOMMANDS: &[&str] = &[
        "machine", "file", "pack", "registry", "auth", "cloud", "config", "rollout",
    ];
    const INTERNAL: &[&str] = &["_boot-vm", "boot-vm", "cuda-daemon", "cuda-clone-worker"];
    let mut words = args
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .filter(|a| is_word(a));
    let top = words.next()?;
    if INTERNAL.contains(&top.as_str()) {
        return None;
    }
    if !WITH_SUBCOMMANDS.contains(&top.as_str()) {
        return Some(top.clone());
    }
    match words.next() {
        Some(sub) => Some(format!("{top} {sub}")),
        None => Some(top.clone()),
    }
}

fn is_word(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c == b'-' || c == b'_')
}

/// Start timing a command. Returns `None` when telemetry is off or the command
/// is internal. Also kicks off a background upload of previously spooled events.
pub fn begin(args: &[String]) -> Option<Session> {
    let command = command_label(args)?;
    if env_disabled() {
        return None;
    }
    let settings = settings_or_init()?;
    if !settings.enabled {
        return None;
    }
    spawn_upload(settings.clone());
    Some(Session {
        command,
        started: Instant::now(),
        settings,
    })
}

/// Record the outcome of a command begun with [`begin`].
pub fn finish(session: Option<Session>, ok: bool) {
    let Some(session) = session else { return };
    // The command itself may have turned telemetry off (`config set telemetry
    // off`); honor that immediately rather than spooling one last event.
    if load_settings().is_some_and(|s| !s.enabled) {
        return;
    }
    let event = Event {
        id: new_install_id(),
        sent_at: chrono::Utc::now().to_rfc3339(),
        install_id: session.settings.install_id,
        cli_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        command: session.command,
        ok,
        duration_ms: session.started.elapsed().as_millis() as u64,
    };
    let _ = spool(&event);
}

fn spool(event: &Event) -> std::io::Result<()> {
    let dir = spool_dir().ok_or_else(|| std::io::Error::other("no home directory"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("pending.jsonl");
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default();
    lines.push(serde_json::to_string(event).map_err(std::io::Error::other)?);
    if lines.len() > MAX_PENDING {
        let drop = lines.len() - MAX_PENDING;
        lines.drain(..drop);
    }
    write_atomic(&path, &lines)
}

fn write_atomic(path: &std::path::Path, lines: &[String]) -> std::io::Result<()> {
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        for line in lines {
            writeln!(file, "{line}")?;
        }
        file.flush()?;
    }
    std::fs::rename(tmp, path)
}

fn read_events(path: &std::path::Path) -> Vec<Event> {
    std::fs::read_to_string(path)
        .map(|t| {
            t.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Where events go: the configured cloud endpoint (so a self-hosted control
/// plane receives its own CLI's telemetry), else the public one.
fn endpoint_and_key() -> (String, Option<String>) {
    let session = smol_cloud::credentials::CliSession::read();
    let base = std::env::var(smol_cloud::credentials::URL_ENV)
        .ok()
        .filter(|v| !v.is_empty())
        .or(session.endpoint)
        .unwrap_or_else(|| smol_cloud::credentials::DEFAULT_BASE_URL.to_string());
    let key = std::env::var(smol_cloud::credentials::TOKEN_ENV)
        .ok()
        .filter(|v| !v.is_empty())
        .or(session.api_key);
    (format!("{}/v1/telemetry", base.trim_end_matches('/')), key)
}

/// Upload the spool on a detached thread. The command never waits for it: if
/// the process exits first, the batch stays in `inflight.jsonl` and the next
/// run picks it up. Spooled lines are only removed after the server accepted them.
fn spawn_upload(_settings: Settings) {
    let Some(dir) = spool_dir() else { return };
    let pending = dir.join("pending.jsonl");
    let inflight = dir.join("inflight.jsonl");
    // Fold a batch abandoned by an earlier run back in front of the queue.
    if inflight.exists() {
        let mut events = read_events(&inflight);
        events.extend(read_events(&pending));
        let lines: Vec<String> = events
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect();
        if write_atomic(&pending, &lines).is_ok() {
            let _ = std::fs::remove_file(&inflight);
        }
    }
    let events = read_events(&pending);
    if events.is_empty() {
        return;
    }
    let (batch, rest) = events.split_at(events.len().min(BATCH));
    let batch = batch.to_vec();
    let rest: Vec<String> = rest
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .collect();
    let inflight_lines: Vec<String> = batch
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .collect();
    if write_atomic(&inflight, &inflight_lines).is_err() || write_atomic(&pending, &rest).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("smol-telemetry".into())
        .spawn(move || {
            let sent = upload(&batch);
            if sent {
                let _ = std::fs::remove_file(&inflight);
            } else {
                // Put the batch back so a later run retries it.
                let mut events = read_events(&inflight);
                events.extend(read_events(&pending));
                let lines: Vec<String> = events
                    .iter()
                    .filter_map(|e| serde_json::to_string(e).ok())
                    .collect();
                if write_atomic(&pending, &lines).is_ok() {
                    let _ = std::fs::remove_file(&inflight);
                }
            }
        });
}

fn upload(batch: &[Event]) -> bool {
    let (url, key) = endpoint_and_key();
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    runtime.block_on(async move {
        let Ok(client) = reqwest::Client::builder().timeout(UPLOAD_TIMEOUT).build() else {
            return false;
        };
        let mut request = client.post(&url).json(&Batch { events: batch });
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        matches!(request.send().await, Ok(response) if response.status().is_success())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("smol")
            .chain(list.iter().copied())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn command_label_keeps_the_command_path_and_nothing_else() {
        assert_eq!(
            command_label(&args(&["run", "alpine", "--", "sh"])),
            Some("run".into())
        );
        assert_eq!(
            command_label(&args(&[
                "machine", "create", "--name", "prod-db", "--image", "pg"
            ])),
            Some("machine create".into())
        );
        assert_eq!(
            command_label(&args(&["--verbose", "machine", "ls"])),
            Some("machine ls".into())
        );
        assert_eq!(command_label(&args(&["machine"])), Some("machine".into()));
        assert_eq!(
            command_label(&args(&["config", "set", "telemetry", "off"])),
            Some("config set".into())
        );
        assert_eq!(command_label(&args(&["_boot-vm", "/tmp/x.json"])), None);
        assert_eq!(
            command_label(&args(&["cuda-daemon", "--socket", "/x"])),
            None
        );
        // A non-word first token is dropped; the next word is what would be
        // recorded (clap rejects such an invocation before it runs anyway).
        assert_eq!(command_label(&args(&["Machine", "ls"])), Some("ls".into()));
        assert_eq!(command_label(&args(&[])), None);
    }

    #[test]
    fn user_values_never_become_a_command() {
        // A second positional that is not a lowercase word is dropped, and for
        // commands without subcommands the second word is never recorded.
        assert_eq!(
            command_label(&args(&["run", "my-secret-image"])),
            Some("run".into())
        );
        assert_eq!(
            command_label(&args(&["machine", "exec", "prod-db", "--", "env"])),
            Some("machine exec".into())
        );
        assert_eq!(
            command_label(&args(&["auth", "login", "user@example.com"])),
            Some("auth login".into())
        );
    }

    #[test]
    fn environment_switches_disable() {
        // Serialize env mutation within this test binary.
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap();
        for (var, value, off) in [
            (CONSENT_ENV, "0", true),
            (CONSENT_ENV, "off", true),
            (CONSENT_ENV, "1", false),
            (DO_NOT_TRACK_ENV, "1", true),
            (DO_NOT_TRACK_ENV, "0", false),
        ] {
            std::env::set_var(var, value);
            assert_eq!(env_disabled(), off, "{var}={value}");
            std::env::remove_var(var);
        }
    }

    #[test]
    fn events_serialize_in_the_wire_shape() {
        let event = Event {
            id: "abc".into(),
            sent_at: "2026-09-22T00:00:00Z".into(),
            install_id: "i".into(),
            cli_version: "1.17.1".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            command: "machine create".into(),
            ok: true,
            duration_ms: 12,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["installId"], "i");
        assert_eq!(json["durationMs"], 12);
        assert_eq!(json["command"], "machine create");
        assert!(json.get("args").is_none());
    }
}
