use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, http::Request, protocol::WebSocketConfig, Message,
};

use crate::{Error, ErrorKind, Result};

#[derive(Clone)]
pub(crate) enum Target {
    Local(SocketAddr),
    Cloud(Request<()>),
}

impl Target {
    pub(crate) fn cloud(base: &str, id: &str, port: u16, key: &str) -> Result<Self> {
        let mut url = reqwest::Url::parse(base)
            .map_err(|_| Error::new(ErrorKind::Config, "Invalid cloud URL"))?;
        let scheme = match url.scheme() {
            "http" => "ws",
            "https" => "wss",
            _ => return Err(Error::new(ErrorKind::Config, "Cloud URL must use HTTP(S)")),
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::new(
                ErrorKind::Config,
                "Cloud URL must not contain credentials",
            ));
        }
        url.set_scheme(scheme).map_err(|_| connection_error())?;
        url.set_query(None);
        url.set_fragment(None);
        url.path_segments_mut()
            .map_err(|_| connection_error())?
            .pop_if_empty()
            .extend(["v1", "machines", id, "tunnel", &port.to_string()]);
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| connection_error())?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {key}")
                .parse()
                .map_err(|_| Error::new(ErrorKind::Config, "Invalid cloud token"))?,
        );
        Ok(Self::Cloud(request))
    }
}

/// A scoped loopback listener forwarding raw TCP to a published machine port.
/// At most 64 clients are active; close or drop disconnects all of them.
#[derive(Debug)]
pub struct Tunnel {
    address: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    error: Arc<Mutex<Option<Error>>>,
}

impl Tunnel {
    pub(crate) fn open(target: Target) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|_| connection_error())?;
        listener
            .set_nonblocking(true)
            .map_err(|_| connection_error())?;
        let address = listener.local_addr().map_err(|_| connection_error())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| connection_error())?;
        let (stop, mut stopped) = oneshot::channel();
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let error = Arc::new(Mutex::new(None));
        let last_error = Arc::clone(&error);
        let worker = std::thread::Builder::new()
            .name("smol-tunnel".into())
            .spawn(move || {
                runtime.block_on(async move {
                    let listener = match tokio::net::TcpListener::from_std(listener) {
                        Ok(listener) => listener,
                        Err(_) => {
                            let _ = ready.send(Err(connection_error()));
                            return;
                        }
                    };
                    let _ = ready.send(Ok(()));
                    let mut clients = JoinSet::new();
                    loop {
                        tokio::select! {
                            biased;
                            _ = &mut stopped => break,
                            result = clients.join_next(), if !clients.is_empty() => {
                                if let Some(Ok(Err(error))) = result {
                                    *last_error.lock().unwrap() = Some(error);
                                }
                            }
                            accepted = listener.accept() => {
                                let Ok((socket, _)) = accepted else { break; };
                                if clients.len() < 64 {
                                    clients.spawn(relay(socket, target.clone()));
                                }
                            }
                        }
                    }
                    clients.abort_all();
                    while clients.join_next().await.is_some() {}
                });
            })
            .map_err(|_| connection_error())?;
        let mut tunnel = Self {
            address,
            stop: Some(stop),
            worker: Some(worker),
            error,
        };
        if let Err(error) = started
            .recv()
            .map_err(|_| connection_error())
            .and_then(|r| r)
        {
            tunnel.close();
            return Err(error);
        }
        Ok(tunnel)
    }

    /// The local address to give an SSH client or another TCP client.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The last upstream failure, if any. Credentials are never included.
    pub fn last_error(&self) -> Option<Error> {
        self.error.lock().unwrap().clone()
    }

    /// Disconnect clients and close the listener. Safe to call more than once.
    pub fn close(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.close();
    }
}

fn connection_error() -> Error {
    Error::new(
        ErrorKind::Connection,
        "TCP tunnel connection failed; check the published port and credentials",
    )
}

async fn relay(mut socket: tokio::net::TcpStream, target: Target) -> Result<()> {
    match target {
        Target::Local(address) => {
            let mut remote = tokio::time::timeout(
                Duration::from_secs(10),
                tokio::net::TcpStream::connect(address),
            )
            .await
            .map_err(|_| connection_error())?
            .map_err(|_| connection_error())?;
            let (mut input, mut output) = socket.split();
            let (mut upstream_input, mut upstream_output) = remote.split();
            tokio::select! {
                r = tokio::io::copy(&mut input, &mut upstream_output) => r,
                r = tokio::io::copy(&mut upstream_input, &mut output) => r,
            }
            .map_err(|_| connection_error())?;
        }
        Target::Cloud(request) => {
            let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|_| connection_error())?
            .with_root_certificates(rustls::RootCertStore::from_iter(
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
            ))
            .with_no_client_auth();
            let config = WebSocketConfig::default()
                .max_message_size(Some(65536))
                .max_frame_size(Some(65536))
                .write_buffer_size(0)
                .max_write_buffer_size(65536 + 1024);
            let connect = tokio_tungstenite::connect_async_tls_with_config(
                request,
                Some(config),
                false,
                Some(tokio_tungstenite::Connector::Rustls(Arc::new(tls))),
            );
            let (ws, _) = tokio::time::timeout(Duration::from_secs(10), connect)
                .await
                .map_err(|_| connection_error())?
                .map_err(|_| connection_error())?;
            let (mut sink, mut stream) = ws.split();
            let (mut reader, mut writer) = socket.split();
            let (ping, mut pong) = mpsc::channel(1);
            let upload = async {
                let mut buffer = [0u8; 32768];
                loop {
                    let message = tokio::select! {
                        data = reader.read(&mut buffer) => {
                            let len = data.map_err(|_| connection_error())?;
                            if len == 0 { return Ok(()); }
                            Message::Binary(buffer[..len].to_vec().into())
                        }
                        Some(data) = pong.recv() => Message::Pong(data),
                    };
                    sink.send(message).await.map_err(|_| connection_error())?;
                }
            };
            let download = async {
                while let Some(message) = stream.next().await {
                    match message.map_err(|_| connection_error())? {
                        Message::Binary(data) => writer
                            .write_all(&data)
                            .await
                            .map_err(|_| connection_error())?,
                        Message::Ping(data) => {
                            ping.send(data).await.map_err(|_| connection_error())?
                        }
                        Message::Pong(_) => {}
                        Message::Close(_) => return Ok(()),
                        _ => return Err(connection_error()),
                    }
                }
                Ok(())
            };
            tokio::select! { result = upload => result, result = download => result }?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    fn client(tunnel: &Tunnel) -> TcpStream {
        let stream = TcpStream::connect(tunnel.address()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
    }

    fn roundtrip(stream: &mut TcpStream) {
        let data = vec![37; 32768];
        let mut received = vec![0; data.len()];
        for _ in 0..32 {
            stream.write_all(&data).unwrap();
            stream.read_exact(&mut received).unwrap();
            assert_eq!(received, data);
        }
    }

    #[test]
    fn local_reconnect_and_scoped_cleanup() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut tunnel = Tunnel::open(Target::Local(server.local_addr().unwrap())).unwrap();
        let worker = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = server.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buf = [0; 32768];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if stream.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        });
        roundtrip(&mut client(&tunnel));
        let mut second = client(&tunnel);
        roundtrip(&mut second);
        let address = tunnel.address();
        tunnel.close();
        tunnel.close();
        assert_eq!(second.read(&mut [0]).unwrap(), 0);
        assert!(TcpStream::connect(address).is_err());
        worker.join().unwrap();
    }

    #[test]
    #[allow(clippy::result_large_err)] // tungstenite's handshake callback requires this error type.
    fn cloud_binary_transport_and_authentication() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = Target::cloud(
            &format!("http://{}", server.local_addr().unwrap()),
            "mach-test",
            22,
            "test-token",
        )
        .unwrap();
        let worker = std::thread::spawn(move || {
            let (stream, _) = server.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut ws = tokio_tungstenite::tungstenite::accept_hdr(
                stream,
                |request: &Request<()>, response| {
                    assert_eq!(request.uri().path(), "/v1/machines/mach-test/tunnel/22");
                    assert_eq!(request.headers()["authorization"], "Bearer test-token");
                    Ok(response)
                },
            )
            .unwrap();
            ws.send(Message::Ping(vec![1].into())).unwrap();
            let mut bytes = 0;
            while bytes < 1024 * 1024 {
                match ws.read().unwrap() {
                    Message::Binary(data) => {
                        bytes += data.len();
                        ws.send(Message::Binary(data)).unwrap();
                    }
                    Message::Pong(_) => {}
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
        });
        let tunnel = Tunnel::open(target).unwrap();
        roundtrip(&mut client(&tunnel));
        worker.join().unwrap();
    }

    #[test]
    fn cloud_rejects_redirects_without_forwarding_credentials() {
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = Target::cloud(
            &format!("http://{}", server.local_addr().unwrap()),
            "m",
            22,
            "private-token",
        )
        .unwrap();
        let address = destination.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = vec![];
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            write!(
                socket,
                "HTTP/1.1 302 Found\r\nLocation: ws://{address}/\r\nContent-Length: 0\r\n\r\n"
            )
            .unwrap();
        });
        let tunnel = Tunnel::open(target).unwrap();
        assert_eq!(client(&tunnel).read(&mut [0]).unwrap(), 0);
        worker.join().unwrap();
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn drop_cancels_an_unfinished_handshake() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = Target::cloud(
            &format!("http://{}", server.local_addr().unwrap()),
            "m",
            22,
            "token",
        )
        .unwrap();
        let tunnel = Tunnel::open(target).unwrap();
        let mut local = client(&tunnel);
        let (mut upstream, _) = server.accept().unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let started = std::time::Instant::now();
        drop(tunnel);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(local.read(&mut [0]).unwrap(), 0);
        let mut pending = Vec::new();
        upstream.read_to_end(&mut pending).unwrap();
    }

    #[test]
    fn invalid_cloud_configuration_is_rejected() {
        for url in ["file:///tmp/socket", "https://user:password@example.com"] {
            assert!(Target::cloud(url, "m", 22, "token").is_err());
        }
        assert!(Target::cloud("https://example.com", "m", 22, "token\r\ninjected: value").is_err());
    }
}
