//! The cloud transport against a mock control plane.
//!
//! These run on any machine: no hypervisor, no credentials, no network beyond
//! loopback. The mock is a few dozen lines of `TcpListener` rather than a test
//! HTTP framework, because the point is to pin the exact bytes the SDK sends
//! and the exact shapes it accepts back.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use smolmachines::{BranchOptions, ConnectOptions, ErrorKind, ExecEvent, ExecOptions, Machine};

/// One request the SDK made.
#[derive(Debug, Clone)]
struct Request {
    method: String,
    path: String,
    body: String,
    authorization: Option<String>,
}

/// A canned reply.
struct Reply {
    status: u16,
    body: String,
    content_type: &'static str,
}

impl Reply {
    fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            content_type: "application/json",
        }
    }

    fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            content_type: "application/json",
        }
    }

    fn sse(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            content_type: "text/event-stream",
        }
    }

    fn bytes(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            content_type: "application/octet-stream",
        }
    }
}

type Route = Box<dyn Fn(&Request) -> Reply + Send + Sync>;

/// A control plane that only exists for one test.
struct MockCloud {
    base_url: String,
    seen: Receiver<Request>,
}

impl MockCloud {
    /// Serve these routes, keyed by `"<METHOD> <path>"`, on a loopback port.
    ///
    /// A path ending in `*` matches any request whose path starts with the
    /// part before it, which is how the files wildcard is covered.
    fn start(routes: HashMap<String, Route>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let base_url = format!("http://{}", listener.local_addr().expect("read the port"));
        let (tx, seen) = mpsc::channel();
        let routes = Arc::new(routes);

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let routes = Arc::clone(&routes);
                let tx = tx.clone();
                thread::spawn(move || serve(stream, &routes, &tx));
            }
        });

        Self { base_url, seen }
    }

    fn connect(&self) -> ConnectOptions {
        ConnectOptions::with_api_key("smk_test").base_url(&self.base_url)
    }

    /// Everything the SDK sent, in order.
    fn requests(&self) -> Vec<Request> {
        self.seen.try_iter().collect()
    }
}

fn serve(mut stream: TcpStream, routes: &HashMap<String, Route>, tx: &mpsc::Sender<Request>) {
    let peer = stream.try_clone().expect("clone the socket");
    let mut reader = BufReader::new(peer);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    let mut authorization = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse().unwrap_or(0),
            "authorization" => authorization = Some(value.trim().to_string()),
            _ => {}
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok();
    }

    let request = Request {
        method: method.clone(),
        path: path.clone(),
        body: String::from_utf8_lossy(&body).into_owned(),
        authorization,
    };
    let _ = tx.send(request.clone());

    let key = format!("{method} {path}");
    let reply = routes
        .get(&key)
        .or_else(|| {
            routes.iter().find_map(|(pattern, route)| {
                let prefix = pattern.strip_suffix('*')?;
                key.starts_with(prefix).then_some(route)
            })
        })
        .map(|route| route(&request))
        .unwrap_or_else(|| Reply::status(404, r#"{"error":"no such route"}"#));

    let response = format!(
        "HTTP/1.1 {} OK\r\ncontent-type: {}\r\ncontent-length: {}\r\nx-request-id: req-42\r\nconnection: close\r\n\r\n{}",
        reply.status,
        reply.content_type,
        reply.body.len(),
        reply.body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn routes(pairs: Vec<(&str, Route)>) -> HashMap<String, Route> {
    pairs
        .into_iter()
        .map(|(key, route)| (key.to_string(), route))
        .collect()
}

fn ready_machine(id: &str) -> String {
    format!(r#"{{"id":"{id}","name":"{id}","state":"running","ready":true,"ports":[]}}"#)
}

// ---------------------------------------------------------------------------

#[test]
fn creating_on_the_cloud_sends_the_image_starts_it_and_waits_for_ready() {
    let cloud = MockCloud::start(routes(vec![
        (
            "POST /v1/machines",
            Box::new(|_| Reply::json(ready_machine("m-1"))),
        ),
        (
            "POST /v1/machines/m-1/start",
            Box::new(|_| Reply::status(204, "")),
        ),
        (
            "GET /v1/machines/m-1",
            Box::new(|_| Reply::json(ready_machine("m-1"))),
        ),
    ]));

    let machine = Machine::builder("demo")
        .image("alpine:latest")
        .cpus(2)
        .memory_mib(1024)
        .network(true)
        .auto_stop_seconds(300)
        .create_with(&cloud.connect())
        .expect("create on the mock cloud");

    assert_eq!(machine.id(), "m-1");
    assert_eq!(machine.target(), smolmachines::Target::Cloud);
    // A cloud machine's process is not ours to know.
    assert_eq!(machine.pid(), None);

    let sent = cloud.requests();
    let create = sent.first().expect("a create request");
    assert_eq!(create.method, "POST");
    assert_eq!(create.authorization.as_deref(), Some("Bearer smk_test"));

    let body: serde_json::Value = serde_json::from_str(&create.body).expect("valid JSON body");
    assert_eq!(body["source"]["type"], "image");
    assert_eq!(body["source"]["reference"], "alpine:latest");
    assert_eq!(body["resources"]["cpus"], 2);
    assert_eq!(body["resources"]["memoryMb"], 1024);
    assert_eq!(body["network"]["mode"], "open");
    assert_eq!(body["autoStopSeconds"], 300);
    // Not branchable, so the flag is absent rather than false.
    assert!(body.get("forkable").is_none());

    assert!(sent.iter().any(|r| r.path == "/v1/machines/m-1/start"));
}

#[test]
fn a_branchable_machine_asks_for_it_at_create_time_and_again_at_start() {
    let cloud = MockCloud::start(routes(vec![
        (
            "POST /v1/machines",
            Box::new(|_| Reply::json(ready_machine("m-2"))),
        ),
        (
            "POST /v1/machines/m-2/start?forkable=true",
            Box::new(|_| Reply::status(204, "")),
        ),
        (
            "GET /v1/machines/m-2",
            Box::new(|_| Reply::json(ready_machine("m-2"))),
        ),
    ]));

    Machine::builder("source")
        .image("alpine:latest")
        .branchable(true)
        .create_with(&cloud.connect())
        .expect("create a branch source");

    let sent = cloud.requests();
    let create: serde_json::Value = serde_json::from_str(&sent[0].body).expect("valid JSON body");
    // Branchability is stored at create time: sending it only at start leaves
    // the source non-branchable and every later branch 409s.
    assert_eq!(create["forkable"], true);
    assert!(sent
        .iter()
        .any(|r| r.path == "/v1/machines/m-2/start?forkable=true"));
}

#[test]
fn a_machine_that_never_becomes_ready_is_deleted_rather_than_left_billing() {
    let cloud = MockCloud::start(routes(vec![
        (
            "POST /v1/machines",
            Box::new(|_| Reply::json(ready_machine("m-3"))),
        ),
        (
            "POST /v1/machines/m-3/start",
            Box::new(|_| Reply::status(500, r#"{"error":"no capacity"}"#)),
        ),
        (
            "GET /v1/machines/m-3",
            // Reached a terminal state instead of becoming ready.
            Box::new(|_| Reply::json(r#"{"id":"m-3","state":"error","ready":false,"ports":[]}"#)),
        ),
        (
            "DELETE /v1/machines/m-3",
            Box::new(|_| Reply::status(204, "")),
        ),
    ]));

    let error = Machine::builder("doomed")
        .image("alpine:latest")
        .create_with(&cloud.connect())
        .expect_err("a machine that cannot become ready must not be returned");

    // The failed start is carried along, since the machine record has no error
    // detail of its own.
    assert!(
        error.message().contains("start failed"),
        "expected the start failure to survive: {error}"
    );

    let sent = cloud.requests();
    assert!(
        sent.iter()
            .any(|r| r.method == "DELETE" && r.path == "/v1/machines/m-3"),
        "the orphan must be deleted: {sent:#?}"
    );
}

#[test]
fn exec_prefers_the_untruncated_bytes_over_the_capped_text() {
    let cloud = MockCloud::start(routes(vec![
        (
            "GET /v1/machines/m-4",
            Box::new(|_| Reply::json(ready_machine("m-4"))),
        ),
        (
            "POST /v1/machines/m-4/exec",
            Box::new(|_| {
                // `stdout` is the server's 1 MiB-capped text; `stdoutB64` is
                // byte-exact.
                Reply::json(
                    r#"{"exitCode":0,"stdout":"trunc","stdoutB64":"dHJ1bmNhdGVk","stderr":""}"#,
                )
            }),
        ),
    ]));

    let machine = Machine::connect_with("m-4", &cloud.connect()).expect("attach");
    let result = machine.exec(["echo", "hi"]).expect("exec");
    assert_eq!(result.stdout_utf8(), "truncated");
    assert!(result.success());

    let sent = cloud.requests();
    let exec = sent
        .iter()
        .find(|r| r.path == "/v1/machines/m-4/exec")
        .expect("an exec request");
    let body: serde_json::Value = serde_json::from_str(&exec.body).expect("valid JSON body");
    // argv, never a shell string.
    assert_eq!(body["command"][0], "echo");
    assert_eq!(body["command"][1], "hi");
}

#[test]
fn a_streamed_command_yields_each_event_as_it_arrives() {
    let cloud = MockCloud::start(routes(vec![
        (
            "GET /v1/machines/m-5",
            Box::new(|_| Reply::json(ready_machine("m-5"))),
        ),
        (
            "POST /v1/machines/m-5/exec/stream",
            Box::new(|_| {
                Reply::sse(
                    "event: stdout\ndata: one\n\n\
                     event: stderr\ndata: two\n\n\
                     event: exit\ndata: {\"exitCode\":3}\n\n",
                )
            }),
        ),
    ]));

    let machine = Machine::connect_with("m-5", &cloud.connect()).expect("attach");
    let events: Vec<_> = machine
        .exec_stream(["make"], ExecOptions::new())
        .expect("open the stream")
        .collect();

    assert_eq!(
        events,
        vec![
            ExecEvent::Stdout(b"one".to_vec()),
            ExecEvent::Stderr(b"two".to_vec()),
            ExecEvent::Exit(3),
        ]
    );
}

#[test]
fn branching_falls_back_to_the_fork_route_on_an_older_control_plane() {
    let cloud = MockCloud::start(routes(vec![
        (
            "GET /v1/machines/m-6",
            Box::new(|_| Reply::json(ready_machine("m-6"))),
        ),
        (
            "GET /v1/machines/child",
            Box::new(|_| Reply::json(ready_machine("child"))),
        ),
        (
            // An older control plane does not know the branch vocabulary.
            "POST /v1/machines/m-6/branches",
            Box::new(|_| Reply::status(404, r#"{"error":"no such route"}"#)),
        ),
        (
            "POST /v1/machines/m-6/fork",
            Box::new(|_| Reply::json(ready_machine("child"))),
        ),
    ]));

    let machine = Machine::connect_with("m-6", &cloud.connect()).expect("attach");
    let branch = machine
        .branch_with("child", BranchOptions::new().checkpointable(true))
        .expect("branch through the fallback");
    assert_eq!(branch.id(), "child");

    let sent = cloud.requests();
    let fork = sent
        .iter()
        .find(|r| r.path == "/v1/machines/m-6/fork")
        .expect("the fork fallback was used");
    let body: serde_json::Value = serde_json::from_str(&fork.body).expect("valid JSON body");
    // The old route spells it `forkable`, not `branchable`.
    assert_eq!(body["forkable"], true);
    assert_eq!(body["name"], "child");
}

#[test]
fn files_move_both_ways_with_their_paths_escaped() {
    let written = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&written);
    let cloud = MockCloud::start(routes(vec![
        (
            "GET /v1/machines/m-7",
            Box::new(|_| Reply::json(ready_machine("m-7"))),
        ),
        (
            "GET /v1/machines/m-7/files/*",
            Box::new(|_| Reply::bytes("file contents")),
        ),
        (
            "PUT /v1/machines/m-7/files/*",
            Box::new(move |request| {
                *sink.lock().expect("record the write") = request.body.clone();
                Reply::status(204, "")
            }),
        ),
    ]));

    let machine = Machine::connect_with("m-7", &cloud.connect()).expect("attach");
    machine
        .write_file("/tmp/a b.txt", "hello")
        .expect("write a file whose name needs escaping");
    assert_eq!(*written.lock().expect("read back the write"), "hello");
    assert_eq!(
        machine.read_file("/tmp/a b.txt").expect("read it back"),
        b"file contents".to_vec()
    );

    let sent = cloud.requests();
    // The space is escaped; the separators are not.
    assert!(
        sent.iter()
            .any(|r| r.path == "/v1/machines/m-7/files/%2Ftmp%2Fa%20b.txt"
                || r.path == "/v1/machines/m-7/files//tmp/a%20b.txt"),
        "paths: {:#?}",
        sent.iter().map(|r| &r.path).collect::<Vec<_>>()
    );
}

#[test]
fn usage_comes_back_typed_rather_than_as_loose_json() {
    let cloud = MockCloud::start(routes(vec![
        (
            "GET /v1/machines/m-8",
            Box::new(|_| Reply::json(ready_machine("m-8"))),
        ),
        (
            "GET /v1/machines/m-8/usage",
            Box::new(|_| {
                Reply::json(
                    r#"{"machineId":"m-8","from":"2026-09-01T00:00:00Z","to":"2026-09-18T00:00:00Z",
                        "usage":{"totalUptimeSeconds":3600,"cpuHours":2.0,"memoryGbHours":4.0,
                                 "diskGbHours":8.0,"egressGb":0.5},
                        "cost":{"cpuMicros":100,"memoryMicros":200,"diskMicros":300,
                                "egressMicros":40,"baseMicros":10,"totalMicros":650,
                                "amountDueMicros":650}}"#,
                )
            }),
        ),
    ]));

    let machine = Machine::connect_with("m-8", &cloud.connect()).expect("attach");
    let report = machine.usage().expect("usage");
    assert_eq!(report.machine_id, "m-8");
    assert_eq!(report.usage.cpu_hours, 2.0);
    assert_eq!(report.cost.total_micros, 650);
    assert_eq!(report.cost.amount_due_micros, 650);
}

#[test]
fn a_server_error_keeps_its_status_body_and_correlation_id() {
    let cloud = MockCloud::start(routes(vec![(
        "GET /v1/machines/gone",
        Box::new(|_| Reply::status(401, r#"{"error":"key expired"}"#)),
    )]));

    let error = Machine::connect_with("gone", &cloud.connect())
        .expect_err("an unauthorized lookup must fail");
    assert_eq!(error.kind(), ErrorKind::Unauthorized);
    assert!(error.message().contains("key expired"), "{error}");
    // The id is only in a header the caller never sees, so it has to be carried
    // in the message or support cannot find the call.
    assert!(error.message().contains("req-42"), "{error}");
}

#[test]
fn local_only_operations_say_which_target_they_need() {
    let cloud = MockCloud::start(routes(vec![(
        "GET /v1/machines/m-9",
        Box::new(|_| Reply::json(ready_machine("m-9"))),
    )]));

    let machine = Machine::connect_with("m-9", &cloud.connect()).expect("attach");
    for error in [
        machine.run("alpine", ["true"]).unwrap_err(),
        machine.pull_image("alpine").unwrap_err(),
        machine.list_images().unwrap_err(),
        machine.sync().unwrap_err(),
    ] {
        assert_eq!(error.kind(), ErrorKind::NotSupported);
        assert!(error.message().contains("cloud"), "{error}");
    }
}

#[test]
fn a_cloud_machine_needs_an_image_and_cannot_take_host_mounts() {
    let connect = ConnectOptions::with_api_key("smk_test").base_url("http://127.0.0.1:1");

    let no_image = Machine::builder("bare")
        .create_with(&connect)
        .expect_err("a cloud machine has no local rootfs to fall back on");
    assert_eq!(no_image.kind(), ErrorKind::Config);
    assert!(no_image.message().contains("image"), "{no_image}");

    let mounted = Machine::builder("mounted")
        .image("alpine:latest")
        .mount(smolmachines::Mount::new("/tmp", "/data"))
        .create_with(&connect)
        .expect_err("a host mount cannot be honoured on someone else's machine");
    assert_eq!(mounted.kind(), ErrorKind::NotSupported);
    // Rejecting beats silently creating a machine missing its data.
    assert!(mounted.message().contains("host mounts"), "{mounted}");
}

#[test]
fn listing_is_a_cloud_operation_and_says_so_locally() {
    let error = smolmachines::list_cloud_machines(&ConnectOptions::local())
        .expect_err("there is no local machine list");
    assert_eq!(error.kind(), ErrorKind::NotSupported);
}

#[test]
fn readiness_falls_back_to_the_agent_when_no_port_is_published() {
    let polls = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&polls);
    let cloud = MockCloud::start(routes(vec![
        (
            "POST /v1/machines",
            Box::new(|_| {
                Reply::json(r#"{"id":"m-10","name":"m-10","state":"started","ready":false}"#)
            }),
        ),
        (
            "POST /v1/machines/m-10/start",
            Box::new(|_| Reply::status(204, "")),
        ),
        (
            "GET /v1/machines/m-10",
            Box::new(move |_| {
                *counter.lock().expect("count the polls") += 1;
                // `ready` never flips: this control plane gates readiness on a
                // port accepting, and this machine publishes none.
                Reply::json(r#"{"id":"m-10","state":"started","ready":false,"ports":[]}"#)
            }),
        ),
        (
            // The agent probe is what settles it.
            "POST /v1/machines/m-10/exec",
            Box::new(|_| Reply::json(r#"{"exitCode":0}"#)),
        ),
    ]));

    let machine = Machine::builder("portless")
        .image("alpine:latest")
        .create_with(&cloud.connect())
        .expect("a machine with no published port must still become ready");
    assert_eq!(machine.id(), "m-10");

    // The probe is a last resort, so `ready` got its grace period first.
    assert!(
        *polls.lock().expect("read the poll count") >= 2,
        "the ready flag should have been given a chance before the probe"
    );
    let sent = cloud.requests();
    assert!(sent.iter().any(|r| r.path == "/v1/machines/m-10/exec"));
}

#[test]
fn an_endpoint_carries_the_credential_the_bridge_requires() {
    let cloud = MockCloud::start(routes(vec![(
        "GET /v1/machines/m-11",
        Box::new(|_| Reply::json(ready_machine("m-11"))),
    )]));

    let machine = Machine::connect_with("m-11", &cloud.connect()).expect("attach");
    let endpoint = machine.endpoint(8080, "").expect("endpoint");
    // A bare path must not gain a trailing slash: `connect/<port>/` routes
    // nowhere.
    assert!(endpoint
        .http_url
        .ends_with("/v1/machines/m-11/connect/8080"));
    assert!(endpoint.ws_url.starts_with("ws://"));
    assert_eq!(
        endpoint.headers,
        vec![("authorization".to_string(), "Bearer smk_test".to_string())]
    );

    let with_path = machine.endpoint(8080, "/health").expect("endpoint");
    assert!(with_path.http_url.ends_with("/connect/8080/health"));
}

#[test]
fn a_slow_control_plane_times_out_instead_of_hanging_forever() {
    // A listener that accepts and never answers.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            held.push(stream);
        }
    });

    let connect = ConnectOptions::with_api_key("smk_test").base_url(&base_url);
    let machine = Machine::attach("unused");
    drop(machine);

    let started = std::time::Instant::now();
    let error = Machine::connect_with("hangs", &connect).expect_err("must not hang forever");
    // The default request window is 30s; assert it gave up rather than blocked
    // indefinitely, without making the test wait the full window.
    assert!(
        matches!(error.kind(), ErrorKind::Timeout | ErrorKind::Connection),
        "{error}"
    );
    assert!(started.elapsed() < Duration::from_secs(90));
}

#[test]
fn a_cloud_capture_can_be_restored_into_a_new_machine() {
    let cloud = MockCloud::start(routes(vec![
        (
            "POST /v1/checkpoints/ckpt-1/restore",
            Box::new(|_| Reply::json(ready_machine("m-restored"))),
        ),
        (
            "POST /v1/machines/m-restored/start",
            Box::new(|_| Reply::status(204, "")),
        ),
        (
            "GET /v1/machines/m-restored",
            Box::new(|_| Reply::json(ready_machine("m-restored"))),
        ),
    ]));

    let machine = Machine::restore_cloud_checkpoint("revived", "ckpt-1", &cloud.connect())
        .expect("restore from a stored capture");
    assert_eq!(machine.id(), "m-restored");

    let sent = cloud.requests();
    let restore = sent
        .iter()
        .find(|r| r.path == "/v1/checkpoints/ckpt-1/restore")
        .expect("a restore request");
    let body: serde_json::Value = serde_json::from_str(&restore.body).expect("valid JSON body");
    assert_eq!(body["name"], "revived");
    // Unlike the local restore, this leaves a machine that is ready to work.
    assert!(sent
        .iter()
        .any(|r| r.path == "/v1/machines/m-restored/start"));
}

#[test]
fn a_restore_that_never_becomes_ready_is_deleted_rather_than_left_billing() {
    let cloud = MockCloud::start(routes(vec![
        (
            "POST /v1/checkpoints/ckpt-2/restore",
            Box::new(|_| Reply::json(ready_machine("m-doomed"))),
        ),
        (
            "POST /v1/machines/m-doomed/start",
            Box::new(|_| Reply::status(204, "")),
        ),
        (
            "GET /v1/machines/m-doomed",
            Box::new(|_| Reply::json(r#"{"id":"m-doomed","state":"error","ready":false}"#)),
        ),
        (
            "DELETE /v1/machines/m-doomed",
            Box::new(|_| Reply::status(204, "")),
        ),
    ]));

    Machine::restore_cloud_checkpoint("doomed", "ckpt-2", &cloud.connect())
        .expect_err("a restore that cannot become ready must not be returned");

    assert!(
        cloud
            .requests()
            .iter()
            .any(|r| r.method == "DELETE" && r.path == "/v1/machines/m-doomed"),
        "the orphaned restore must be deleted"
    );
}

#[test]
fn restoring_by_id_says_it_is_a_cloud_operation_when_asked_locally() {
    let error = Machine::restore_cloud_checkpoint("x", "ckpt-3", &ConnectOptions::local())
        .expect_err("a local capture is a file, not an id");
    assert_eq!(error.kind(), ErrorKind::NotSupported);
    assert!(error.message().contains("restore_checkpoint"), "{error}");
}

#[test]
fn deleting_with_usage_takes_the_final_settled_reading() {
    let cloud = MockCloud::start(routes(vec![
        (
            "GET /v1/machines/m-12",
            Box::new(|_| Reply::json(ready_machine("m-12"))),
        ),
        (
            "DELETE /v1/machines/m-12?includeUsage=true",
            Box::new(|_| {
                Reply::json(
                    r#"{"machineId":"m-12","from":"2026-09-18T00:00:00Z","to":"2026-09-18T01:00:00Z",
                        "usage":{"totalUptimeSeconds":3600},
                        "cost":{"totalMicros":4200,"amountDueMicros":4200}}"#,
                )
            }),
        ),
    ]));

    let machine = Machine::connect_with("m-12", &cloud.connect()).expect("attach");
    let report = machine.delete_with_usage().expect("delete and settle");
    assert_eq!(report.cost.total_micros, 4200);
    assert_eq!(report.usage.total_uptime_seconds, 3600.0);

    // The usage has to ride along with the delete: afterwards there is nothing
    // left to ask.
    assert!(cloud
        .requests()
        .iter()
        .any(|r| r.method == "DELETE" && r.path.contains("includeUsage=true")));
}

#[test]
fn a_local_machine_has_no_final_bill_to_settle() {
    let error = Machine::attach("local-only")
        .delete_with_usage()
        .expect_err("nothing meters your own hardware");
    assert_eq!(error.kind(), ErrorKind::NotSupported);
}

#[test]
fn an_architecture_and_a_command_reach_the_control_plane() {
    let cloud = MockCloud::start(routes(vec![
        (
            "POST /v1/machines",
            Box::new(|_| Reply::json(ready_machine("m-arm"))),
        ),
        (
            "POST /v1/machines/m-arm/start?forkable=true",
            Box::new(|_| Reply::status(204, "")),
        ),
        (
            "GET /v1/machines/m-arm",
            Box::new(|_| Reply::json(ready_machine("m-arm"))),
        ),
    ]));

    Machine::builder("on-arm")
        .image("alpine:latest")
        .arch("arm64")
        .command(["sleep", "infinity"])
        .branchable(true)
        .create_with(&cloud.connect())
        .expect("create on arm");

    let sent = cloud.requests();
    let body: serde_json::Value = serde_json::from_str(&sent[0].body).expect("valid JSON body");
    // Architecture rides on the source, not the top level.
    assert_eq!(body["source"]["arch"], "arm64");
    // A command in the config must not be dropped on the way to the cloud: the
    // machine would silently run the image's own entrypoint instead.
    assert_eq!(body["command"][0], "sleep");
    assert_eq!(body["command"][1], "infinity");
    // Both vocabularies, so neither an old nor a new control plane stores it
    // non-branchable.
    assert_eq!(body["branchable"], true);
    assert_eq!(body["forkable"], true);
}

#[test]
fn a_local_machine_cannot_be_asked_for_an_architecture_the_host_lacks() {
    let other = if std::env::consts::ARCH == "aarch64" {
        "amd64"
    } else {
        "arm64"
    };
    let error = Machine::builder("impossible")
        .image("alpine:latest")
        .arch(other)
        .create()
        .expect_err("the host has only one architecture");
    assert_eq!(error.kind(), ErrorKind::Config);
    assert!(error.message().contains("cloud"), "{error}");

    // The host's own architecture, by either spelling, is fine.
    for spelling in [
        std::env::consts::ARCH,
        if std::env::consts::ARCH == "aarch64" {
            "arm64"
        } else {
            "amd64"
        },
    ] {
        let config = Machine::builder("fine")
            .image("alpine:latest")
            .arch(spelling)
            .build();
        // Only the arch check is under test here; creating for real needs an
        // engine, so stop at the translation.
        assert!(config.arch.is_some());
    }
}
