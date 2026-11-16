use super::*;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
};

fn upload(url: &str, parts: usize) -> CheckpointUpload {
    serde_json::from_value(serde_json::json!({
        "checkpoint":{"id":"ckpt-owned","machineId":"","status":"uploading","sizeBytes":parts,"arch":"amd64","createdAt":"now"},
        "partSizeBytes":1,"uploadUrls":(0..parts).map(|i| format!("{url}/part-{i}?signature=private")).collect::<Vec<_>>(),"expiresAt":"later"
    })).unwrap()
}
fn client() -> Client {
    Client::new(Credentials::new("http://127.0.0.1:9", "test-access"))
        .unwrap()
        .with_tenant("t-team")
        .unwrap()
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(e) => panic!("upload did not connect: {e}"),
        }
    }
}
fn read_headers(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 8192);
    }
    let headers = String::from_utf8(bytes).unwrap().to_lowercase();
    assert!(!headers.contains("authorization:"));
    assert!(!headers.contains("smol-tenant:"));
    headers
}
fn read_part(stream: &mut TcpStream) {
    assert!(read_headers(stream).contains("content-length: 1"));
    stream.read_exact(&mut [0]).unwrap();
}

#[test]
fn cancellation_closes_all_active_requests_and_does_not_start_remaining_parts() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let path = std::env::temp_dir().join(format!(
        "cancel-upload-{}",
        listener.local_addr().unwrap().port()
    ));
    std::fs::write(&path, b"abcdefgh").unwrap();
    let token = StreamCancel::new();
    let stopped = token.clone();
    let sending_path = path.clone();
    let allocation = upload(&url, 8);
    let sending = thread::spawn(move || {
        client().send_checkpoint_upload_cancellable(
            &sending_path,
            &allocation,
            &mut |_, _| {},
            &stopped,
        )
    });
    let mut streams = Vec::new();
    for _ in 0..UPLOAD_PARALLELISM {
        let mut stream = accept(&listener);
        read_part(&mut stream);
        streams.push(stream);
    }
    // Every body arrived, but no response: cancellation must interrupt all
    // stalled response waits, not merely stop the next file read.
    let start = Instant::now();
    token.cancel();
    let error = sending.join().unwrap().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Cancelled);
    assert!(start.elapsed() < Duration::from_secs(2));
    for mut stream in streams {
        let result = stream.read(&mut [0]);
        assert!(
            matches!(result, Ok(0))
                || result.is_err_and(|e| matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ))
        );
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn cancellation_interrupts_retry_backoff_and_a_new_upload_has_its_own_token() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let path = std::env::temp_dir().join(format!(
        "cancel-retry-{}",
        listener.local_addr().unwrap().port()
    ));
    std::fs::write(&path, b"a").unwrap();
    let token = StreamCancel::new();
    let stop = token.clone();
    let sending_path = path.clone();
    let allocation = upload(&url, 1);
    let (done, result) = mpsc::channel();
    let sending = thread::spawn(move || {
        done.send(client().send_checkpoint_upload_cancellable(
            &sending_path,
            &allocation,
            &mut |_, _| {},
            &stop,
        ))
        .unwrap()
    });
    let mut first = accept(&listener);
    read_part(&mut first);
    first
        .write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .unwrap();
    drop(first);
    thread::sleep(Duration::from_millis(100));
    token.cancel();
    assert_eq!(
        result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err()
            .kind(),
        ErrorKind::Cancelled
    );
    sending.join().unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let sending_path = path.clone();
    let allocation = upload(&url, 1);
    let next = thread::spawn(move || {
        client().send_checkpoint_upload(&sending_path, &allocation, &mut |_, _| {})
    });
    let mut second = accept(&listener);
    read_part(&mut second);
    second
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .unwrap();
    drop(second);
    next.join().unwrap().unwrap();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn cancellation_before_upload_does_not_open_a_connection_or_read_the_file() {
    let token = StreamCancel::new();
    token.cancel();
    let result = client().send_checkpoint_upload_cancellable(
        std::path::Path::new("/missing-checkpoint"),
        &upload("http://127.0.0.1:9", 1),
        &mut |_, _| panic!("stopped upload reports no progress"),
        &token,
    );
    assert_eq!(result.unwrap_err().kind(), ErrorKind::Cancelled);
}

#[test]
fn cancellation_stops_a_body_blocked_by_a_slow_reader() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let path = std::env::temp_dir().join(format!(
        "cancel-body-{}",
        listener.local_addr().unwrap().port()
    ));
    let size = 64 * 1024 * 1024;
    std::fs::File::create(&path).unwrap().set_len(size).unwrap();
    let mut allocation = upload(&url, 1);
    allocation.checkpoint.size_bytes = size;
    allocation.part_size_bytes = size;
    let token = StreamCancel::new();
    let stop = token.clone();
    let sending_path = path.clone();
    let (done, result) = mpsc::channel();
    let sending = thread::spawn(move || {
        done.send(client().send_checkpoint_upload_cancellable(
            &sending_path,
            &allocation,
            &mut |_, _| {},
            &stop,
        ))
        .unwrap()
    });
    let mut stream = accept(&listener);
    assert!(read_headers(&mut stream).contains(&format!("content-length: {size}")));
    // Hold back the body long enough for the client's socket buffer to fill.
    thread::sleep(Duration::from_millis(100));
    token.cancel();
    assert_eq!(
        result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err()
            .kind(),
        ErrorKind::Cancelled
    );
    sending.join().unwrap();
    let mut received = 0;
    let mut bytes = [0; 65536];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) => break,
            Ok(n) => received += n as u64,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                break
            }
            Err(e) => panic!("cancelled request remained open: {e}"),
        }
    }
    assert!(
        received < size,
        "stop must interrupt the body before all bytes are sent"
    );
    std::fs::remove_file(path).unwrap();
}
