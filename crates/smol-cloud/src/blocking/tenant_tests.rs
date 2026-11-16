use super::*;
use std::io::{Read, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

type Reply = (u16, String, String);
struct Server {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new(reply: impl Fn(usize, &str) -> Reply + Send + 'static) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("accept: {e}"),
                };
                // macOS inherits O_NONBLOCK from the listening socket; the
                // request reader below is intentionally blocking with a deadline.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let n = stream.read(&mut buffer).unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    assert!(bytes.len() < 1024 * 1024);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .map(|n| n.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                let index = {
                    let mut log = log.lock().unwrap();
                    let index = log.len();
                    log.push(request.clone());
                    index
                };
                let (status, headers, body) = reply(index, &request);
                if status == 0 {
                    // Model a committed request whose response is lost.
                    continue;
                }
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn rows(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
    fn client(&self) -> Client {
        Client::new(Credentials::new(&self.url, "test-key"))
            .unwrap()
            .with_tenant("tenant-selected")
            .unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}
fn scoped(request: &str) {
    let headers = request.split("\r\n\r\n").next().unwrap().to_lowercase();
    assert!(headers.contains("\r\nsmol-tenant: tenant-selected"));
    assert!(headers.contains("\r\nauthorization: bearer test-key"));
}

#[test]
fn tracked_restore_lost_reply_is_looked_up_without_replaying_creation() {
    for status in [0, 503] {
        let api = Server::new(move |n, request| {
            scoped(request);
            match n {
                0 => {
                    assert!(request.starts_with("POST /v1/checkpoints/ckpt%20owned/restore "));
                    let body: serde_json::Value =
                        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                    assert_eq!(
                        body,
                        serde_json::json!({"name":"my app", "network":{"mode":"open"}, "requestId":"move_123-abc"})
                    );
                    (status, "Retry-After: 0\r\n".into(), String::new())
                }
                1 => {
                    assert!(request
                        .starts_with("GET /v1/checkpoints/ckpt%20owned/restores/move_123-abc "));
                    (200, String::new(), serde_json::json!({"checkpointId":"ckpt owned","requestId":"move_123-abc","machineId":"mach-committed","pendingCheckpointId":"ckpt owned"}).to_string())
                }
                _ => panic!("recovery must not repeat creation or start the machine"),
            }
        });
        let client = api.client();
        assert!(client
            .restore_checkpoint_tracked(
                "ckpt owned",
                "my app",
                &Network {
                    mode: Some("open".into()),
                    cidrs: vec![],
                    hosts: vec![]
                },
                "move_123-abc"
            )
            .is_err());
        assert_eq!(api.rows().len(), 1);
        let receipt = client
            .checkpoint_restore("ckpt owned", "move_123-abc")
            .unwrap();
        assert_eq!(receipt.machine_id, "mach-committed");
        assert_eq!(receipt.pending_checkpoint_id.as_deref(), Some("ckpt owned"));
        assert_eq!(api.rows().len(), 2);
    }
}

#[test]
fn restore_receipt_errors_never_adopt_inventory_or_submit_a_restore() {
    let other = Server::new(|_, _| panic!("must not forward account credentials"));
    let location = format!("Location: {}/receipt\r\n", other.url);
    let api = Server::new(move |n, request| {
        scoped(request);
        assert!(request.starts_with("GET /v1/checkpoints/ckpt-owned/restores/move_123 "));
        ([404, 403, 302][n], location.clone(), String::new())
    });
    let client = api.client();
    for status in [404, 403, 302] {
        assert_eq!(
            client
                .checkpoint_restore("ckpt-owned", "move_123")
                .unwrap_err()
                .status(),
            Some(status)
        );
    }
    assert_eq!(api.rows().len(), 3);
    assert!(other.rows().is_empty());
}

#[test]
fn restore_receipts_check_identity_and_preserve_consumed_checkpoint_state() {
    let api = Server::new(|n, request| {
        scoped(request);
        let (checkpoint, key, machine, pending) = match n {
            0 => ("ckpt-other", "move_123", "mach-owned", Some("ckpt-owned")),
            1 => ("ckpt-owned", "move_other", "mach-owned", Some("ckpt-owned")),
            2 => ("ckpt-owned", "move_123", "", Some("ckpt-owned")),
            3 => ("ckpt-owned", "move_123", "mach-owned", Some("ckpt-other")),
            4 => ("ckpt-owned", "move_123", "mach-owned", None),
            _ => panic!("unexpected request"),
        };
        (200, String::new(), serde_json::json!({"checkpointId":checkpoint,"requestId":key,"machineId":machine,"pendingCheckpointId":pending}).to_string())
    });
    let client = api.client();
    for _ in 0..4 {
        assert!(client
            .checkpoint_restore("ckpt-owned", "move_123")
            .unwrap_err()
            .to_string()
            .contains("mismatched"));
    }
    let consumed = client.checkpoint_restore("ckpt-owned", "move_123").unwrap();
    assert!(consumed.pending_checkpoint_id.is_none());
    assert_eq!(consumed.machine_id, "mach-owned");
}

#[test]
fn invalid_restore_keys_are_refused_before_network_access() {
    let api = Server::new(|_, _| panic!("invalid key must not be sent"));
    let client = api.client();
    for key in [
        "".to_string(),
        "x".repeat(129),
        "wrong/key".into(),
        "a b".into(),
        "é".into(),
        "a\r\nb".into(),
    ] {
        assert!(client.checkpoint_restore("ckpt-owned", &key).is_err());
        assert!(client.start_checkpoint_restore("ckpt-owned", &key).is_err());
        assert!(client.checkpoint_execution("ckpt-owned", &key).is_err());
        assert!(client
            .restore_checkpoint_tracked_with_idle_action(
                "ckpt-owned",
                "copy",
                &Network {
                    mode: Some("blocked".into()),
                    cidrs: vec![],
                    hosts: vec![]
                },
                &key,
                IdleAction::Pause
            )
            .is_err());
        assert!(client
            .restore_checkpoint_tracked(
                "ckpt-owned",
                "copy",
                &Network {
                    mode: Some("blocked".into()),
                    cidrs: vec![],
                    hosts: vec![]
                },
                &key
            )
            .is_err());
    }
    assert!(api.rows().is_empty());
}

#[test]
fn guarded_restore_start_never_falls_back_to_an_ordinary_start() {
    for status in [0, 200, 403, 404, 409, 503] {
        let api = Server::new(move |n, request| {
            assert_eq!(n, 0, "a failure must not fall back to a cold start");
            scoped(request);
            assert!(
                request.starts_with("POST /v1/checkpoints/ckpt%20owned/restores/move_123/start ")
            );
            (
                status,
                String::new(),
                r#"{"id":"mach-owned","state":"started"}"#.into(),
            )
        });
        let result = api
            .client()
            .start_checkpoint_restore("ckpt owned", "move_123");
        if status == 200 {
            assert_eq!(result.unwrap().id, "mach-owned");
        } else if status == 0 {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap_err().status(), Some(status));
        }
        assert_eq!(api.rows().len(), 1);
    }
}

#[test]
fn checkpoint_upload_restore_and_cleanup_keep_the_account_off_object_storage() {
    let objects = Server::new(|_, request| {
        assert!(request.starts_with("PUT /part "));
        assert!(request.ends_with("data"));
        assert!(!request.to_lowercase().contains("authorization:"));
        assert!(!request.to_lowercase().contains("smol-tenant:"));
        (200, String::new(), String::new())
    });
    let part = format!("{}/part", objects.url);
    let checkpoint = serde_json::json!({"id":"ckpt-owned","machineId":"source","status":"available","sizeBytes":4,"arch":"arm64","createdAt":"now"});
    let api = Server::new(move |n, request| {
        scoped(request);
        match n {
            0 => {
                assert!(request.starts_with("POST /v1/checkpoints "));
                (201, String::new(), serde_json::json!({"checkpoint":checkpoint,"partSizeBytes":4,"uploadUrls":[part],"expiresAt":"later"}).to_string())
            }
            1 => {
                assert!(request.starts_with("POST /v1/checkpoints/ckpt-owned/complete "));
                (200, String::new(), checkpoint.to_string())
            }
            2 => {
                assert!(request.starts_with("POST /v1/checkpoints/ckpt-owned/restore "));
                (
                    201,
                    String::new(),
                    r#"{"id":"mach-owned","state":"stopped"}"#.into(),
                )
            }
            3 => {
                assert!(request.starts_with("DELETE /v1/machines/mach-owned "));
                (204, String::new(), String::new())
            }
            4 => {
                assert!(request.starts_with("DELETE /v1/checkpoints/ckpt-owned "));
                (204, String::new(), String::new())
            }
            _ => panic!("unexpected request"),
        }
    });
    let path = std::env::temp_dir().join(format!(
        "tenant-upload-{}-{}",
        std::process::id(),
        api.url.rsplit(':').next().unwrap()
    ));
    std::fs::write(&path, b"data").unwrap();
    let client = api.client();
    let checkpoint = client.upload_checkpoint(&path, &mut |_, _| {}).unwrap();
    std::fs::remove_file(&path).unwrap();
    let machine = client
        .clone()
        .restore_checkpoint(&checkpoint.id, "copy")
        .unwrap();
    client.delete(&machine.id).unwrap();
    client.delete_checkpoint(&checkpoint.id).unwrap();
    assert_eq!(api.rows().len(), 5);
    assert_eq!(objects.rows().len(), 1);
}

#[test]
fn account_refusals_and_redirects_do_not_fall_back_or_forward() {
    let other = Server::new(|_, _| (200, String::new(), "[]".into()));
    let location = format!("Location: {}/v1/machines\r\n", other.url);
    let api = Server::new(move |n, request| {
        scoped(request);
        match n {
            0 => (403, String::new(), "account refused".into()),
            1 => (302, location.clone(), String::new()),
            2 => (503, "Retry-After: 0\r\n".into(), String::new()),
            3 => (200, String::new(), "[]".into()),
            _ => panic!("unexpected fallback"),
        }
    });
    let client = api.client();
    assert_eq!(client.machines().unwrap_err().status(), Some(403));
    assert_eq!(client.machines().unwrap_err().status(), Some(302));
    assert!(client.clone().machines().unwrap().is_empty());
    assert_eq!(api.rows().len(), 4);
    assert!(other.rows().is_empty());
    for tenant in ["", " ", "team\r\nAuthorization: bad", "team name", "é"] {
        assert!(Client::new(Credentials::new(&api.url, "test-key"))
            .unwrap()
            .with_tenant(tenant)
            .is_err());
    }
}

#[test]
fn token_renewal_keeps_the_selected_account_without_scoping_the_identity_provider() {
    let issuer = Server::new(|_, request| {
        assert!(request.starts_with("POST /oauth/token "));
        assert!(!request.to_lowercase().contains("smol-tenant:"));
        (200, String::new(), r#"{"access_token":"renewed-key","refresh_token":"renewed-refresh","expires_in":86400}"#.into())
    });
    let api = Server::new(|n, request| {
        let headers = request.to_lowercase();
        assert!(headers.contains("\r\nsmol-tenant: tenant-selected"));
        if n == 0 {
            assert!(headers.contains("\r\nauthorization: bearer test-key"));
            (401, String::new(), String::new())
        } else {
            assert!(headers.contains("\r\nauthorization: bearer renewed-key"));
            (200, String::new(), "[]".into())
        }
    });
    let dir = std::env::temp_dir().join(format!(
        "tenant-renew-{}-{}",
        std::process::id(),
        api.url.rsplit(':').next().unwrap()
    ));
    std::fs::create_dir(&dir).unwrap();
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        "[cloud]\napi_key = \"test-key\"\nrefresh_token = \"test-refresh\"\n",
    )
    .unwrap();
    let credentials =
        Credentials::test_session(&api.url, "test-key", "test-refresh", &config, &issuer.url);
    let client = Client::new(credentials)
        .unwrap()
        .with_tenant("tenant-selected")
        .unwrap();
    assert!(client.machines().unwrap().is_empty());
    assert_eq!(api.rows().len(), 2);
    assert_eq!(issuer.rows().len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn plain_and_resumable_streams_keep_the_account_through_cleanup() {
    let command = Command {
        command: vec!["true".into()],
        ..Default::default()
    };
    let plain = Server::new(|_, request| {
        scoped(request);
        assert!(request.starts_with("POST /v1/machines/m/exec/stream "));
        (
            200,
            "Content-Type: text/event-stream\r\n".into(),
            "event: exit\ndata: {\"exitCode\":0}\n\n".into(),
        )
    });
    let events: Vec<_> = plain
        .client()
        .exec_stream_plain("m", &command, &StreamCancel::new())
        .unwrap()
        .collect();
    assert_eq!(events, vec![StreamEvent::Exit(0)]);
    let resumable = Server::new(|n, request| {
        scoped(request);
        if n == 0 {
            assert!(request.starts_with("POST "));
            (201, String::new(), r#"{"execId":"x","state":"running","done":false,"output":[],"next":0,"start":0,"end":0}"#.into())
        } else if n == 1 {
            assert!(request.starts_with("GET "));
            (200, String::new(), r#"{"execId":"x","state":"exited","done":true,"exitCode":0,"output":[],"next":0,"start":0,"end":0}"#.into())
        } else {
            assert!(request.starts_with("DELETE "));
            (204, String::new(), String::new())
        }
    });
    let events: Vec<_> = resumable
        .client()
        .exec_stream("m", &command)
        .unwrap()
        .collect();
    assert_eq!(events, vec![StreamEvent::Exit(0)]);
    assert_eq!(resumable.rows().len(), 3);
}

#[test]
fn staged_upload_records_ownership_before_sending_parts_and_reconciles_completion() {
    let objects = Server::new(|_, request| {
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        let headers = headers.to_lowercase();
        assert!(!headers.contains("authorization:"));
        assert!(!headers.contains("smol-tenant:"));
        if request.starts_with("PUT /0 ") {
            assert_eq!(body, "abcd");
        } else if request.starts_with("PUT /1 ") {
            assert_eq!(body, "efgh");
        } else {
            assert!(request.starts_with("PUT /2 "));
            assert_eq!(body, "ij");
        }
        (200, String::new(), String::new())
    });
    let urls: Vec<_> = (0..3).map(|n| format!("{}/{n}", objects.url)).collect();
    let checkpoint = serde_json::json!({"id":"ckpt-owned","machineId":"source","status":"available","sizeBytes":10,"arch":"arm64","createdAt":"now"});
    let api = Server::new(move |n, request| {
        scoped(request);
        match n {
            0 => {
                assert!(request.starts_with("POST /v1/checkpoints "));
                assert!(request.contains("\"sizeBytes\":10"));
                assert!(request.contains("\"image\":\"alpine\""));
                (201, String::new(), serde_json::json!({"checkpoint":checkpoint,"partSizeBytes":4,"uploadUrls":urls,"expiresAt":"later"}).to_string())
            }
            1 => {
                assert!(request.starts_with("POST /v1/checkpoints/ckpt-owned/complete "));
                // A proxy can fail after the server committed completion.
                (502, String::new(), "gateway failed".into())
            }
            2 => {
                assert!(request.starts_with("GET /v1/checkpoints/ckpt-owned "));
                (200, String::new(), checkpoint.to_string())
            }
            3 => {
                assert!(request.starts_with("DELETE /v1/checkpoints/ckpt-owned "));
                (204, String::new(), String::new())
            }
            _ => panic!("unexpected request"),
        }
    });
    let path = std::env::temp_dir().join(format!(
        "staged-upload-{}",
        api.url.rsplit(':').next().unwrap()
    ));
    std::fs::write(&path, b"abcdefghij").unwrap();
    let client = api.client().with_max_retries(0);
    let upload = client
        .begin_checkpoint_upload(&path, Some("alpine"))
        .unwrap();
    // The desktop can durably record ownership here, before any part is sent.
    assert_eq!(upload.checkpoint.id, "ckpt-owned");
    assert!(objects.rows().is_empty());
    assert_eq!(api.rows().len(), 1);
    let mut last = (0, 0);
    client
        .send_checkpoint_upload(&path, &upload, &mut |sent, total| {
            assert!(sent <= total);
            last = (sent, total);
        })
        .unwrap();
    assert_eq!(last, (10, 10));
    assert_eq!(objects.rows().len(), 3);
    assert_eq!(api.rows().len(), 1, "sending does not finalize");
    assert_eq!(
        client
            .complete_checkpoint_upload(&upload.checkpoint.id)
            .unwrap_err()
            .status(),
        Some(502)
    );
    let recovered = client.get_checkpoint(&upload.checkpoint.id).unwrap();
    assert_eq!(recovered.status, "available");
    client.delete_checkpoint(&upload.checkpoint.id).unwrap();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn incomplete_upload_can_be_cleaned_up_after_source_disappears() {
    let objects = Server::new(|_, _| panic!("no file data should be sent"));
    let part = format!("{}/part", objects.url);
    let checkpoint = serde_json::json!({"id":"ckpt-owned","machineId":"source","status":"uploading","sizeBytes":4,"arch":"arm64","createdAt":"now"});
    let api = Server::new(move |n, request| {
        scoped(request);
        match n {
            0 => (201, String::new(), serde_json::json!({"checkpoint":checkpoint,"partSizeBytes":4,"uploadUrls":[part],"expiresAt":"later"}).to_string()),
            1 => {
                assert!(request.starts_with("DELETE /v1/checkpoints/ckpt-owned "));
                (204, String::new(), String::new())
            }
            _ => panic!("a failed send must not complete or allocate again"),
        }
    });
    let path = std::env::temp_dir().join(format!(
        "lost-upload-{}",
        api.url.rsplit(':').next().unwrap()
    ));
    std::fs::write(&path, b"data").unwrap();
    let client = api.client();
    let upload = client.begin_checkpoint_upload(&path, None).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(client
        .send_checkpoint_upload(&path, &upload, &mut |_, _| {})
        .is_err());
    client.delete_checkpoint(&upload.checkpoint.id).unwrap();
    assert_eq!(api.rows().len(), 2);
    assert!(objects.rows().is_empty());
}

#[test]
fn malformed_upload_layout_and_changed_files_fail_before_object_requests() {
    let objects = Server::new(|_, _| panic!("invalid layout must not send bytes"));
    let api = Server::new(|_, _| panic!("validation does not call the control plane"));
    let path = std::env::temp_dir().join(format!(
        "bad-upload-{}",
        api.url.rsplit(':').next().unwrap()
    ));
    std::fs::write(&path, b"data").unwrap();
    let mut upload: CheckpointUpload = serde_json::from_value(serde_json::json!({
        "checkpoint":{"id":"ckpt-owned","machineId":"source","status":"uploading","sizeBytes":4,"arch":"arm64","createdAt":"now"},
        "partSizeBytes":4,"uploadUrls":[format!("{}/part",objects.url)],"expiresAt":"later"
    })).unwrap();
    let client = api.client();
    for (size, urls) in [(0, 1), (1, 1), (4, 0), (4, 2), (u64::MAX, 2)] {
        upload.part_size_bytes = size;
        upload.upload_urls = vec![format!("{}/part", objects.url); urls];
        assert!(client
            .send_checkpoint_upload(&path, &upload, &mut |_, _| {})
            .unwrap_err()
            .message()
            .contains("part layout"));
    }
    upload.part_size_bytes = 4;
    upload.upload_urls.truncate(1);
    std::fs::write(&path, b"changed").unwrap();
    assert!(client
        .send_checkpoint_upload(&path, &upload, &mut |_, _| {})
        .unwrap_err()
        .message()
        .contains("size changed"));
    std::fs::write(&path, []).unwrap();
    assert!(client.begin_checkpoint_upload(&path, None).is_err());
    assert!(client
        .begin_checkpoint_upload(&std::env::temp_dir(), None)
        .is_err());
    std::fs::remove_file(path).unwrap();
    assert!(objects.rows().is_empty());
    assert!(api.rows().is_empty());
}

fn download_fixture(size: u64) -> Checkpoint {
    Checkpoint {
        id: "ckpt-saved".into(),
        machine_id: "machine-owned".into(),
        status: "available".into(),
        size_bytes: size,
        arch: "amd64".into(),
        created_at: "2026-11-16T00:00:00Z".into(),
        download_url: Some("https://not-the-control.invalid/secret?signature=private".into()),
    }
}

#[test]
fn saved_execution_and_download_use_the_selected_account_and_exact_checkpoint() {
    let server = Server::new(|i, request| {
        scoped(request);
        if i == 0 {
            assert!(request.starts_with("GET /v1/machines/machine-owned/pause "));
            (200, String::new(), serde_json::json!({"machineId":"machine-owned","state":"paused","wakeOnRequest":false,"checkpoint": download_fixture(4)}).to_string())
        } else {
            assert!(request.starts_with("GET /v1/checkpoints/ckpt-saved/download "));
            (200, String::new(), "data".into())
        }
    });
    let client = server.client();
    let saved = client.saved_execution("machine-owned").unwrap();
    assert_eq!(saved.machine_id, "machine-owned");
    assert_eq!(saved.state, "paused");
    assert!(!saved.wake_on_request);
    let checkpoint = saved.checkpoint.unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("saved.smolcheckpoint");
    let mut progress = Vec::new();
    client
        .download_checkpoint_cancellable(
            &checkpoint,
            &path,
            &mut |a, b| progress.push((a, b)),
            &StreamCancel::new(),
        )
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"data");
    assert_eq!(progress.last(), Some(&(4, 4)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
            0
        );
    }
    assert_eq!(
        client
            .download_checkpoint_cancellable(
                &checkpoint,
                &path,
                &mut |_, _| {},
                &StreamCancel::new()
            )
            .unwrap_err()
            .kind(),
        ErrorKind::Conflict
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"data");
    assert_eq!(
        server.rows().len(),
        2,
        "an existing destination causes no request"
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn checkpoint_download_refuses_size_changes_redirects_and_cancelled_publication() {
    for (size, status, body, stop) in [
        (5, 200, "data", false),
        (3, 200, "data", false),
        (4, 302, "data", false),
        (4, 200, "data", true),
    ] {
        let server = Server::new(move |_, request| {
            scoped(request);
            (
                status,
                "Location: https://not-the-control.invalid/?token=private\r\n".into(),
                body.into(),
            )
        });
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("saved");
        let cancel = StreamCancel::new();
        let error = server
            .client()
            .download_checkpoint_cancellable(
                &download_fixture(size),
                &path,
                &mut |received, _| {
                    if stop && received > 0 {
                        cancel.cancel();
                    }
                },
                &cancel,
            )
            .unwrap_err();
        if stop {
            assert_eq!(error.kind(), ErrorKind::Cancelled);
        }
        assert!(!error.to_string().contains("private"));
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        assert_eq!(server.rows().len(), 1);
    }
}

#[test]
fn checkpoint_download_cancels_a_stalled_body_and_removes_partial_bytes() {
    use std::sync::mpsc;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sent, received) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            request.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        scoped(&request);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nfirst")
            .unwrap();
        sent.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(
            stream.read(&mut byte).unwrap_or(0),
            0,
            "cancel closes the connection"
        );
    });
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("saved");
    let dest = path.clone();
    let cancel = StreamCancel::new();
    let stopping = cancel.clone();
    let worker = std::thread::spawn(move || {
        let client = Client::new(Credentials::new(format!("http://{address}"), "test-key"))
            .unwrap()
            .with_tenant("tenant-selected")
            .unwrap();
        client.download_checkpoint_cancellable(
            &download_fixture(100),
            &dest,
            &mut |_, _| {},
            &stopping,
        )
    });
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    let start = Instant::now();
    cancel.cancel();
    assert_eq!(
        worker.join().unwrap().unwrap_err().kind(),
        ErrorKind::Cancelled
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    server.join().unwrap();
    assert!(!path.exists());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn checkpoint_download_checks_stream_length_without_content_length() {
    for body in ["short", "too-many-bytes"] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
            }
            write!(stream, "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{body}").unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("checkpoint");
        let client =
            Client::new(Credentials::new(format!("http://{address}"), "test-key")).unwrap();
        let error = client
            .download_checkpoint_cancellable(
                &download_fixture(10),
                &destination,
                &mut |_, _| {},
                &StreamCancel::new(),
            )
            .unwrap_err();
        assert!(error.to_string().contains(if body == "short" {
            "incomplete"
        } else {
            "exceeds"
        }));
        server.join().unwrap();
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

#[test]
fn checkpoint_download_does_not_replace_a_concurrent_destination() {
    let server = Server::new(|_, request| {
        scoped(request);
        (200, String::new(), "data".into())
    });
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("checkpoint");
    server
        .client()
        .download_checkpoint_cancellable(
            &download_fixture(4),
            &destination,
            &mut |received, _| {
                if received == 4 {
                    std::fs::write(&destination, b"keep this").unwrap();
                }
            },
            &StreamCancel::new(),
        )
        .unwrap_err();
    assert_eq!(std::fs::read(&destination).unwrap(), b"keep this");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn execution_observation_never_uses_auto_start_and_validates_the_receipt_and_boot() {
    for status in [200, 403, 404, 409, 502] {
        let server = Server::new(move |_, request| {
            scoped(request);
            assert!(request
                .starts_with("GET /v1/checkpoints/ckpt%20owned/restores/move_123/execution "));
            (status, String::new(), serde_json::json!({"checkpointId":"ckpt owned","requestId":"move_123","machineId":"machine-owned","bootId":"7f0f9472-3741-48c6-899b-92817f81bedc"}).to_string())
        });
        let result = server
            .client()
            .with_max_retries(0)
            .checkpoint_execution("ckpt owned", "move_123");
        assert_eq!(result.is_ok(), status == 200);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
    for (field, value) in [
        ("checkpointId", "other"),
        ("requestId", "other"),
        ("machineId", ""),
        ("bootId", ""),
        ("bootId", "00000000-0000-0000-0000-000000000000"),
        ("bootId", "7f0f9472x3741-48c6-899b-92817f81bedc"),
    ] {
        let server = Server::new(move |_, _| {
            let mut reply = serde_json::json!({"checkpointId":"cp","requestId":"move_123","machineId":"machine-owned","bootId":"7f0f9472-3741-48c6-899b-92817f81bedc"});
            reply[field] = value.into();
            (200, String::new(), reply.to_string())
        });
        assert!(server
            .client()
            .checkpoint_execution("cp", "move_123")
            .is_err());
    }
}

#[test]
fn tracked_idle_pause_preserves_the_created_id_and_never_retries_with_stop() {
    for (status, idle) in [
        (200, Some("pause")),
        (200, Some("stop")),
        (200, None),
        (422, None),
        (503, None),
        (0, None),
    ] {
        let api = Server::new(move |n, request| {
            assert_eq!(n, 0, "no retry or stop-policy fallback");
            scoped(request);
            assert!(request.starts_with("POST /v1/checkpoints/ckpt%20owned/restore "));
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(
                body,
                serde_json::json!({"name":"my app","network":{"mode":"blocked"},"requestId":"move_123","idleAction":"pause"})
            );
            (
                status,
                String::new(),
                serde_json::json!({"id":"mach-committed","state":"stopped","idleAction":idle})
                    .to_string(),
            )
        });
        let result = api.client().restore_checkpoint_tracked_with_idle_action(
            "ckpt owned",
            "my app",
            &Network {
                mode: Some("blocked".into()),
                cidrs: vec![],
                hosts: vec![],
            },
            "move_123",
            IdleAction::Pause,
        );
        if status == 200 {
            let machine = result.unwrap();
            assert_eq!(
                machine.id, "mach-committed",
                "callers must retain ownership even if a server ignored the policy"
            );
            assert_eq!(machine.idle_action.as_deref(), idle);
        } else {
            assert!(result.is_err());
        }
        assert_eq!(api.rows().len(), 1);
    }
}

#[test]
fn execution_observation_exposes_actual_or_unknown_idle_policy() {
    for idle in [None, Some("stop"), Some("pause"), Some("future-policy")] {
        let api = Server::new(move |_, request| {
            scoped(request);
            (200,String::new(),serde_json::json!({"checkpointId":"cp","requestId":"move_123","machineId":"machine-owned","bootId":"7f0f9472-3741-48c6-899b-92817f81bedc","idleAction":idle}).to_string())
        });
        let observed = api.client().checkpoint_execution("cp", "move_123").unwrap();
        assert_eq!(observed.idle_action.as_deref(), idle);
    }
}

#[test]
fn guarded_resume_claims_the_new_pause_without_retry_or_fallback() {
    for status in [200, 404, 409, 503, 0] {
        let api = Server::new(move |n, request| {
            assert_eq!(n, 0, "resume is never replayed");
            scoped(request);
            assert!(request
                .starts_with("POST /v1/checkpoints/original%20capture/restores/move_123/resume "));
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(body, serde_json::json!({"checkpointId":"new-pause"}));
            (
                status,
                String::new(),
                r#"{"id":"owned","state":"started","idleAction":"pause"}"#.into(),
            )
        });
        let result = api.client().with_max_retries(5).resume_checkpoint_restore(
            "original capture",
            "move_123",
            "new-pause",
        );
        assert_eq!(result.is_ok(), status == 200);
        assert_eq!(api.rows().len(), 1);
    }
}

#[test]
fn restore_receipt_distinguishes_later_pause_and_refuses_contradictory_sources() {
    for (pending, paused, valid) in [
        (None, None, true),
        (None, Some("new-pause"), true),
        (Some("original"), None, true),
        (Some("original"), Some("new-pause"), false),
        (None, Some(""), false),
        (None, Some(" "), false),
    ] {
        let api = Server::new(move |_, request| {
            scoped(request);
            (200, String::new(), serde_json::json!({"checkpointId":"original","requestId":"move_123","machineId":"owned","pendingCheckpointId":pending,"pausedCheckpointId":paused}).to_string())
        });
        let result = api.client().checkpoint_restore("original", "move_123");
        assert_eq!(result.is_ok(), valid);
        if valid {
            assert_eq!(result.unwrap().paused_checkpoint_id.as_deref(), paused);
        }
    }
    let client = Client::new(Credentials::new("http://127.0.0.1:1", "unused")).unwrap();
    for saved in ["".to_string(), " ".to_string(), "x".repeat(257)] {
        assert!(client
            .resume_checkpoint_restore("cp", "move_123", &saved)
            .unwrap_err()
            .to_string()
            .contains("expected saved checkpoint"));
    }
}
