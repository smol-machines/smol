//! Foreground, caller-owned observation; no socket or detached service.
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    io::{self, Read, Write},
    sync::{mpsc, Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

const PERIOD: Duration = Duration::from_secs(5);
const MAX_FRAME: usize = 4 * 1024 * 1024;
type State = Result<Option<Value>, String>;

#[derive(Default)]
struct Signal {
    revision: u64,
    stopped: bool,
}
#[derive(Default)]
struct Control(Mutex<Signal>, Condvar);
impl Control {
    fn refresh(&self) {
        let mut state = self.0.lock().unwrap();
        state.revision = state.revision.wrapping_add(1);
        self.1.notify_all();
    }
    fn stop(&self) {
        self.0.lock().unwrap().stopped = true;
        self.1.notify_all();
    }
    fn revision(&self) -> Option<u64> {
        let state = self.0.lock().unwrap();
        (!state.stopped).then_some(state.revision)
    }
    fn wait(&self, revision: u64, remaining: Duration) {
        let state = self.0.lock().unwrap();
        drop(
            self.1
                .wait_timeout_while(state, remaining, |state| {
                    !state.stopped && state.revision == revision
                })
                .unwrap(),
        );
    }
}
struct StopOnDrop(Arc<Control>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}
enum Message {
    Frame(Vec<u8>),
    Failed(anyhow::Error),
    End,
}

pub(super) fn run(names: &[String]) -> anyhow::Result<()> {
    let names: Vec<_> = names
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    anyhow::ensure!(!names.is_empty(), "At least one machine name is required");
    let control = Arc::new(Control::default());
    let _stop = StopOnDrop(control.clone());
    let (send, receive) = mpsc::sync_channel(names.len());
    let input_control = control.clone();
    let input_send = send.clone();
    thread::Builder::new()
        .name("observation-input".into())
        .spawn(move || {
            let mut input = io::stdin().lock();
            let mut bytes = [0; 1024];
            loop {
                match input.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(_) => input_control.refresh(),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            input_control.stop();
            let _ = input_send.send(Message::End);
        })?;
    // Each machine has an independent bounded probe and freshness clock. The
    // CLI process owns these threads; EOF exits without waiting for a busy
    // guest, and process exit closes all of their connections.
    let _workers = spawn_observers(names, Arc::new(super::local_watch_row), control, send)?;
    let mut output = io::stdout().lock();
    for message in receive {
        match message {
            Message::End => break,
            Message::Failed(error) => return Err(error),
            Message::Frame(bytes) => {
                output.write_all(&bytes)?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
        }
    }
    Ok(())
}

fn spawn_observers<F>(
    names: Vec<String>,
    provider: Arc<F>,
    control: Arc<Control>,
    send: mpsc::SyncSender<Message>,
) -> anyhow::Result<Vec<thread::JoinHandle<()>>>
where
    F: Fn(&str) -> anyhow::Result<Option<Value>> + Send + Sync + 'static,
{
    names
        .into_iter()
        .map(|name| {
            let (provider, control, send) = (provider.clone(), control.clone(), send.clone());
            Ok(thread::Builder::new()
                .name("machine-observation".into())
                .spawn(move || {
                    let mut observation = Observation::default();
                    while let Some(revision) = control.revision() {
                        let started = Instant::now();
                        let state = provider(&name)
                            .map_err(|_| "Cannot read local machine state.".to_owned());
                        let frame = match observation.frame(&name, state) {
                            Ok(bytes) => Message::Frame(bytes),
                            Err(error) => {
                                let _ = send.send(Message::Failed(error));
                                return;
                            }
                        };
                        if send.send(frame).is_err() {
                            return;
                        }
                        // Probe time belongs to the interval, so busy agents
                        // do not accumulate drift beyond the freshness window.
                        control.wait(revision, PERIOD.saturating_sub(started.elapsed()));
                    }
                })?)
        })
        .collect()
}

#[derive(Default)]
struct Observation {
    previous: Option<State>,
    sequence: u64,
}
impl Observation {
    fn frame(&mut self, name: &str, state: State) -> anyhow::Result<Vec<u8>> {
        let changed = self.previous.as_ref() != Some(&state);
        if changed {
            self.sequence = self
                .sequence
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("Observation sequence exhausted"))?;
        }
        let mut frame = json!({"version":1,"name":name,"sequence":self.sequence});
        if changed {
            match &state {
                Ok(machine) => frame["snapshot"] = json!({"machine":machine}),
                Err(error) => frame["error"] = json!(error),
            }
        }
        let bytes = serde_json::to_vec(&frame)?;
        anyhow::ensure!(
            bytes.len() < MAX_FRAME,
            "Local machine observation exceeds 4 MiB"
        );
        self.previous = Some(state);
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_heartbeats_deletion_and_error_recovery_keep_per_name_sequence() {
        let mut observation = Observation::default();
        let states = [
            Ok(Some(json!({"name":"app","created_at":1}))),
            Ok(Some(json!({"name":"app","created_at":1}))),
            Err("unavailable".into()),
            Err("unavailable".into()),
            Ok(None),
            Ok(Some(json!({"name":"app","created_at":2}))),
        ];
        let frames: Vec<Value> = states
            .into_iter()
            .map(|s| serde_json::from_slice(&observation.frame("app", s).unwrap()).unwrap())
            .collect();
        assert_eq!(
            frames
                .iter()
                .map(|f| f["sequence"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [1, 1, 2, 2, 3, 4]
        );
        assert_eq!(frames[0]["snapshot"]["machine"]["created_at"], 1);
        assert!(frames[1].get("snapshot").is_none());
        assert_eq!(frames[2]["error"], "unavailable");
        assert!(frames[3].get("error").is_none());
        assert_eq!(frames[4]["snapshot"], json!({"machine":null}));
        assert_eq!(frames[5]["snapshot"]["machine"]["created_at"], 2);
    }

    #[test]
    fn slow_peer_does_not_block_healthy_observation_or_refresh() {
        let control = Arc::new(Control::default());
        let (send, receive) = mpsc::sync_channel(8);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = gate.clone();
        let (entered, started) = mpsc::channel();
        let workers = spawn_observers(
            vec!["slow".into(), "ready".into()],
            Arc::new(move |name: &str| {
                if name == "slow" {
                    let _ = entered.send(());
                    drop(
                        worker_gate
                            .1
                            .wait_while(worker_gate.0.lock().unwrap(), |released| !*released)
                            .unwrap(),
                    );
                }
                Ok(Some(json!({"name":name})))
            }),
            control.clone(),
            send,
        )
        .unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let parse = |message| match message {
            Message::Frame(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            _ => panic!("unexpected observer result"),
        };
        let first = parse(receive.recv_timeout(Duration::from_secs(2)).unwrap());
        assert_eq!(first["name"], "ready");
        assert_eq!(first["snapshot"]["machine"]["name"], "ready");
        control.refresh();
        let next = parse(receive.recv_timeout(Duration::from_secs(2)).unwrap());
        assert_eq!(next["name"], "ready");
        assert_eq!(next["sequence"], 1);
        assert!(next.get("snapshot").is_none());
        control.stop();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        for worker in workers {
            worker.join().unwrap();
        }
    }

    #[test]
    fn oversize_frame_is_rejected_before_publication() {
        let mut observation = Observation::default();
        assert!(observation
            .frame(
                "app",
                Ok(Some(json!({"name":"app","data":"x".repeat(MAX_FRAME)})))
            )
            .is_err());
        assert!(observation.previous.is_none());
    }
}
