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
            worker.join().unwrap();
        }
    }
}
fn scoped(request: &str) {
    let headers = request.split("\r\n\r\n").next().unwrap().to_lowercase();
    assert!(headers.contains("\r\nsmol-tenant: tenant-selected"));
    assert!(headers.contains("\r\nauthorization: bearer test-key"));
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
    let checkpoint = serde_json::json!({"id":"ckpt-owned","machineId":"source","status":"ready","sizeBytes":4,"arch":"arm64","createdAt":"now"});
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
