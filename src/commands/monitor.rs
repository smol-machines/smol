//! smol monitor — supervise a machine with health checks and a restart policy.
//!
//! Faithful port of the engine's `machine monitor` over the public smolvm lib.
//! Lifecycle actions (start/stop/restart) reuse smol's own StartCmd/StopCmd so
//! there's a single start path. Runs in the foreground until Ctrl+C.

use super::start::StartCmd;
use super::stop::StopCmd;
use clap::Args;
use smolvm::agent::{AgentClient, AgentManager};
use std::time::Duration;

#[derive(Args, Debug)]
pub struct MonitorCmd {
    /// Machine to monitor (default: "default")
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// Override restart policy (never, always, on-failure, unless-stopped)
    #[arg(long, value_name = "POLICY")]
    pub restart: Option<String>,

    /// Health check command (run inside the VM via sh -c)
    #[arg(long, value_name = "CMD")]
    pub health_cmd: Option<String>,

    /// Health check timeout in seconds
    #[arg(long, default_value = "5", value_name = "SECS")]
    pub health_timeout: u64,

    /// Check interval in seconds
    #[arg(long, default_value = "5", value_name = "SECS")]
    pub interval: u64,

    /// Health check failures before triggering restart
    #[arg(long, default_value = "3", value_name = "N")]
    pub health_retries: u32,
}

fn restart_machine(name: &str) -> anyhow::Result<()> {
    StartCmd {
        name: Some(name.to_string()),
        cloud: false,
        local: false,
        forkable: false,
    }
    .run()
}

fn stop_machine(name: &str) {
    let _ = StopCmd {
        name: Some(name.to_string()),
        cloud: false,
        local: false,
    }
    .run();
}

impl MonitorCmd {
    pub fn run(self) -> anyhow::Result<()> {
        use smolvm::config::{RecordState, RestartPolicy};
        use smolvm::db::SmolvmDb;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let name = self.name.clone().unwrap_or_else(|| "default".to_string());

        let db = SmolvmDb::open()?;
        let record = db
            .get_vm(&name)?
            .ok_or_else(|| anyhow::anyhow!("machine '{name}' not found"))?;

        // Restart config: CLI override > record.
        let mut restart = record.restart.clone();
        if let Some(ref policy_str) = self.restart {
            restart.policy = policy_str
                .parse::<RestartPolicy>()
                .map_err(|e| anyhow::anyhow!("--restart: {e}"))?;
        }

        // Health check + timing: CLI override > record.
        let health_cmd = self
            .health_cmd
            .clone()
            .map(|c| vec!["sh".into(), "-c".into(), c])
            .or_else(|| record.health_cmd.clone());
        let health_timeout =
            Duration::from_secs(record.health_timeout_secs.unwrap_or(self.health_timeout));
        let health_retries = record.health_retries.unwrap_or(self.health_retries);
        let interval = Duration::from_secs(record.health_interval_secs.unwrap_or(self.interval));
        let startup_grace = record
            .health_startup_grace_secs
            .map(Duration::from_secs)
            .unwrap_or(Duration::ZERO);
        drop(db);

        // Ensure running.
        let manager = AgentManager::for_vm(&name)
            .map_err(|e| anyhow::anyhow!("create agent manager: {e}"))?;
        if !manager.is_process_alive() {
            println!("Machine '{name}' is not running, starting...");
            restart_machine(&name)?;
        }

        println!(
            "Monitoring machine '{name}' (policy: {}, interval: {}s)",
            restart.policy,
            interval.as_secs()
        );
        if health_cmd.is_some() {
            println!(
                "  Health check: retries={health_retries}, timeout={}s",
                health_timeout.as_secs()
            );
        }

        // Ctrl+C handler via SIGINT.
        //
        // SAFETY: `stop` is an Arc<AtomicBool> alive for the whole function; the
        // cloned Arc keeps the pointee valid for the loop's lifetime. The handler
        // only does an async-signal-safe atomic store.
        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            unsafe {
                let _ = libc::signal(libc::SIGINT, {
                    static mut STOP_FLAG: *const AtomicBool = std::ptr::null();
                    STOP_FLAG = Arc::as_ptr(&stop);
                    extern "C" fn handler(_: libc::c_int) {
                        unsafe {
                            if !STOP_FLAG.is_null() {
                                (*STOP_FLAG).store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    handler as *const () as libc::sighandler_t
                });
            }
        }

        let mut consecutive_health_failures: u32 = 0;
        let mut last_check = std::time::Instant::now();
        let mut last_start = std::time::Instant::now();

        loop {
            std::thread::sleep(interval);
            if stop.load(Ordering::SeqCst) {
                break;
            }

            // Suspend detection: a wall-clock gap much larger than the interval
            // means the host slept; skip a cycle to let the VM recover.
            let elapsed = last_check.elapsed();
            last_check = std::time::Instant::now();
            if elapsed > interval * 3 {
                println!(
                    "  detected suspend (~{}s) — skipping health check for recovery",
                    elapsed.as_secs().saturating_sub(interval.as_secs())
                );
                consecutive_health_failures = 0;
                continue;
            }

            // Refresh manager to pick up PID changes after a restart.
            let manager = match AgentManager::for_vm(&name) {
                Ok(m) => m,
                Err(_) => continue,
            };

            if manager.is_process_alive() {
                if !startup_grace.is_zero() && last_start.elapsed() < startup_grace {
                    continue;
                }
                if let Some(ref cmd) = health_cmd {
                    match AgentClient::connect_with_short_timeout(manager.vsock_socket()) {
                        Ok(mut client) => {
                            match client.vm_exec(
                                cmd.clone(),
                                vec![],
                                None,
                                Some(health_timeout),
                                None,
                            ) {
                                Ok((0, _, _)) => {
                                    if consecutive_health_failures > 0 {
                                        println!("  health check passed (recovered)");
                                    }
                                    consecutive_health_failures = 0;
                                }
                                Ok((code, _, stderr)) => {
                                    consecutive_health_failures += 1;
                                    println!(
                                        "  health check failed (exit {code}, {consecutive_health_failures}/{health_retries}): {}",
                                        String::from_utf8_lossy(&stderr).trim()
                                    );
                                }
                                Err(e) => {
                                    consecutive_health_failures += 1;
                                    println!("  health check error ({consecutive_health_failures}/{health_retries}): {e}");
                                }
                            }
                            if consecutive_health_failures >= health_retries {
                                println!("  unhealthy — stopping machine for restart");
                                stop_machine(&name);
                                continue;
                            }
                        }
                        Err(_) => {
                            consecutive_health_failures += 1;
                            println!("  cannot connect to agent ({consecutive_health_failures}/{health_retries})");
                        }
                    }
                }
            } else {
                // Machine is dead.
                consecutive_health_failures = 0;
                let exit_code = manager.child_pid().and_then(smolvm::process::try_wait);
                println!(
                    "  machine exited (exit code: {})",
                    exit_code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into())
                );

                if let Ok(db) = SmolvmDb::open() {
                    let _ = db.update_vm(&name, |r| {
                        r.state = RecordState::Stopped;
                        r.pid = None;
                        r.last_exit_code = exit_code;
                    });
                }

                if restart.should_restart(exit_code) {
                    let backoff = restart.backoff_duration();
                    restart.restart_count += 1;
                    println!(
                        "  restarting (attempt {}, backoff {}s)...",
                        restart.restart_count,
                        backoff.as_secs()
                    );
                    if let Ok(db) = SmolvmDb::open() {
                        let _ = db.update_vm(&name, |r| {
                            r.restart.restart_count = restart.restart_count;
                        });
                    }
                    std::thread::sleep(backoff);
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    match restart_machine(&name) {
                        Ok(()) => {
                            println!("  machine restarted");
                            last_start = std::time::Instant::now();
                        }
                        Err(e) => println!("  restart failed: {e}"),
                    }
                } else {
                    println!(
                        "  not restarting (policy: {}, count: {}/{})",
                        restart.policy,
                        restart.restart_count,
                        if restart.max_retries > 0 {
                            restart.max_retries.to_string()
                        } else {
                            "unlimited".into()
                        }
                    );
                    break;
                }
            }
        }

        println!("Monitor stopped.");
        Ok(())
    }
}
