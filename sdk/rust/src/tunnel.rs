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

mod http;
mod target;
pub use target::{TunnelStream, TunnelTarget};

#[derive(Clone)]
pub(crate) enum Target {
    Local(SocketAddr),
    Cloud(Request<()>),
}

impl Target {
    pub(crate) fn cloud(base: &str, id: &str, port: u16, key: &str) -> Result<Self> {
        if id.is_empty() || port == 0 {
            return Err(Error::new(
                ErrorKind::Config,
                "Machine ID and port are required",
            ));
        }
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

    fn from_client(client: &smol_cloud::blocking::Client, id: &str, port: u16) -> Result<Self> {
        let credentials = client.credentials();
        credentials.renew_if_expiring()?;
        let Self::Cloud(mut request) =
            Self::cloud(credentials.base_url(), id, port, &credentials.api_key())?
        else {
            unreachable!()
        };
        if let Some(tenant) = client.tenant() {
            request.headers_mut().insert(
                "Smol-Tenant",
                tenant.parse().map_err(|_| connection_error())?,
            );
        }
        Ok(Self::Cloud(request))
    }
}

/// A scoped loopback listener forwarding raw TCP to a published machine port.
/// At most 64 clients are active; close or drop disconnects all of them.
#[derive(Debug)]
pub struct Tunnel {
    address: SocketAddr,
    browser_url: Option<String>,
    routes: mpsc::Sender<(Option<Target>, std::sync::mpsc::SyncSender<()>)>,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    error: Arc<Mutex<Option<Error>>>,
}

impl Tunnel {
    pub(crate) fn open(target: Target) -> Result<Self> {
        Self::open_at(0, Some(target), None)
    }

    /// Reserve a loopback port, initially with traffic suspended. Pass zero
    /// to allocate a port, or a saved port to retain an app's address after a
    /// restart. An occupied port fails without choosing a different address.
    pub fn bind(port: u16) -> Result<Self> {
        Self::open_at(port, None, None)
    }

    /// Reserve a browser-facing HTTP address, initially suspended. Requests
    /// must use this listener's exact `name.localhost:port` Host and, when present,
    /// same-origin Origin/Fetch Metadata headers. HTTP bodies stream without
    /// buffering in full; WebSocket upgrades are supported. CONNECT, forward
    /// proxy requests, and other upgrade protocols are rejected.
    ///
    /// Give each app a unique, stable lowercase DNS label (up to 63 ASCII
    /// letters, digits or hyphens) and open [`Self::browser_url`]. Separate
    /// names give apps separate cookie hosts, unlike ports on one IP address.
    /// The browser must resolve `*.localhost` to loopback. The address is private to
    /// this computer and exists only while this handle is alive. This does not
    /// authenticate other local processes or publish a public cloud URL.
    pub fn bind_http(port: u16, name: &str) -> Result<Self> {
        if name.is_empty()
            || name.len() > 63
            || !name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            || name.starts_with('-')
            || name.ends_with('-')
        {
            return Err(Error::new(
                ErrorKind::Config,
                "Choose a lowercase app DNS label",
            ));
        }
        Self::open_at(port, None, Some(name))
    }

    fn open_at(port: u16, mut target: Option<Target>, http_name: Option<&str>) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|_| connection_error())?;
        listener
            .set_nonblocking(true)
            .map_err(|_| connection_error())?;
        let address = listener.local_addr().map_err(|_| connection_error())?;
        let http_authority = http_name.map(|name| format!("{name}.localhost:{}", address.port()));
        let browser_url = http_authority
            .as_ref()
            .map(|authority| format!("http://{authority}/"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| connection_error())?;
        let (stop, mut stopped) = oneshot::channel();
        let (routes, mut changes) =
            mpsc::channel::<(Option<Target>, std::sync::mpsc::SyncSender<()>)>(1);
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
                            Some((destination, applied)) = changes.recv() => {
                                // No old connection may keep writing to the previous
                                // machine after a move or an account change.
                                clients.abort_all();
                                while clients.join_next().await.is_some() {}
                                target = destination;
                                *last_error.lock().unwrap() = None;
                                let _ = applied.send(());
                            }
                            result = clients.join_next(), if !clients.is_empty() => {
                                if let Some(Ok(Err(error))) = result {
                                    *last_error.lock().unwrap() = Some(error);
                                }
                            }
                            accepted = listener.accept() => {
                                let Ok((socket, _)) = accepted else { break; };
                                if let Some(target) = &target {
                                    if clients.len() < 64 {
                                        let target = target.clone();
                                        let http_authority = http_authority.clone();
                                        clients.spawn(async move {
                                            if let Some(authority) = http_authority {
                                                http::serve(socket, target, authority).await
                                            } else {
                                                relay(socket, target).await
                                            }
                                        });
                                    }
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
            browser_url,
            routes,
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

    /// Keep the listener's address and forward new connections to a local
    /// published port. Existing connections are closed before this returns.
    /// Only loopback targets are accepted; this does not publish an app publicly.
    pub fn route_local(&mut self, address: SocketAddr) -> Result<()> {
        if !address.ip().is_loopback() || address.port() == 0 || address == self.address {
            return Err(Error::new(
                ErrorKind::Config,
                "Choose a different loopback app port",
            ));
        }
        self.route(Some(Target::Local(address)))
    }

    /// Keep the listener's address and forward new connections through the
    /// cloud's authenticated TCP bridge, preserving the client's selected account.
    /// Existing connections close before this returns. Call again to refresh
    /// credentials or change destinations; this never starts a machine itself.
    pub fn route_cloud(
        &mut self,
        client: &smol_cloud::blocking::Client,
        machine_id: &str,
        port: u16,
    ) -> Result<()> {
        self.route(Some(Target::from_client(client, machine_id, port)?))
    }

    /// Disconnect clients and stop forwarding while retaining the local port.
    /// Incoming connections close until a new route is supplied, so a paused
    /// transfer source cannot be woken by traffic through this tunnel.
    pub fn suspend(&mut self) -> Result<()> {
        self.route(None)
    }

    fn route(&mut self, target: Option<Target>) -> Result<()> {
        let (applied, result) = std::sync::mpsc::sync_channel(1);
        self.routes
            .try_send((target, applied))
            .map_err(|_| connection_error())?;
        result.recv().map_err(|_| connection_error())
    }

    /// The local address to give an SSH client or another TCP client.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The browser URL for an HTTP tunnel, or `None` for a raw TCP tunnel.
    pub fn browser_url(&self) -> Option<&str> {
        self.browser_url.as_deref()
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

async fn relay<S>(mut socket: S, target: Target) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut remote = TunnelTarget { target }.connect().await?;
    tokio::io::copy_bidirectional(&mut socket, &mut remote)
        .await
        .map_err(|_| connection_error())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    fn accept(listener: &TcpListener) -> TcpStream {
        listener.set_nonblocking(true).unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    return stream;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < until =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        }
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn address_survives_local_cloud_and_return_with_account_scope() {
        let local = TcpListener::bind("127.0.0.1:0").unwrap();
        let local_address = local.local_addr().unwrap();
        let local_worker = std::thread::spawn(move || {
            for _ in 0..2 {
                let mut stream = accept(&local);
                let mut buf = [0; 32768];
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    stream.write_all(&buf[..n]).unwrap();
                }
            }
        });
        let cloud = TcpListener::bind("127.0.0.1:0").unwrap();
        let cloud_client = smol_cloud::blocking::Client::new(smol_cloud::Credentials::new(
            format!("http://{}", cloud.local_addr().unwrap()),
            "private-account-token",
        ))
        .unwrap()
        .with_tenant("team-original")
        .unwrap();
        let (check_suspended, suspended) = std::sync::mpsc::sync_channel(1);
        let cloud_worker = std::thread::spawn(move || {
            let mut ws = tokio_tungstenite::tungstenite::accept_hdr(
                accept(&cloud),
                |request: &Request<()>, response| {
                    assert_eq!(
                        request.uri().path(),
                        "/v1/machines/moved-machine/tunnel/8080"
                    );
                    assert_eq!(
                        request.headers()["authorization"],
                        "Bearer private-account-token"
                    );
                    assert_eq!(request.headers()["smol-tenant"], "team-original");
                    Ok(response)
                },
            )
            .unwrap();
            let mut bytes = 0;
            while bytes < 1024 * 1024 {
                match ws.read().unwrap() {
                    Message::Binary(data) => {
                        bytes += data.len();
                        ws.send(Message::Binary(data)).unwrap();
                    }
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
            suspended.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(
                cloud.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        });
        let mut tunnel = Tunnel::bind(0).unwrap();
        let address = tunnel.address();
        assert_eq!(client(&tunnel).read(&mut [0]).unwrap(), 0);
        tunnel.route_local(local_address).unwrap();
        let mut old = client(&tunnel);
        roundtrip(&mut old);
        assert!(tunnel.route_local(address).is_err());
        assert!(tunnel.route_cloud(&cloud_client, "", 8080).is_err());
        roundtrip(&mut old);
        tunnel
            .route_cloud(&cloud_client, "moved-machine", 8080)
            .unwrap();
        assert_eq!(old.read(&mut [0]).unwrap(), 0);
        assert_eq!(tunnel.address(), address);
        assert!(!format!("{tunnel:?}").contains("private-account-token"));
        roundtrip(&mut client(&tunnel));
        tunnel.suspend().unwrap();
        assert_eq!(client(&tunnel).read(&mut [0]).unwrap(), 0);
        check_suspended.send(()).unwrap();
        tunnel.route_local(local_address).unwrap();
        assert_eq!(tunnel.address(), address);
        roundtrip(&mut client(&tunnel));
        tunnel.close();
        assert!(tunnel.suspend().is_err());
        local_worker.join().unwrap();
        cloud_worker.join().unwrap();
    }

    #[test]
    fn reserved_port_refuses_collisions_and_invalid_routes_leave_it_usable() {
        let mut tunnel = Tunnel::bind(0).unwrap();
        let address = tunnel.address();
        assert!(Tunnel::bind(address.port()).is_err());
        for target in [
            address,
            "127.0.0.1:0".parse().unwrap(),
            "192.0.2.1:80".parse().unwrap(),
        ] {
            assert!(tunnel.route_local(target).is_err());
        }
        assert_eq!(tunnel.address(), address);
        tunnel.suspend().unwrap();
        tunnel.close();
        let rebound = Tunnel::bind(address.port()).unwrap();
        assert_eq!(rebound.address(), address);
    }

    #[test]
    fn local_half_close_keeps_the_response_readable() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let worker_address = server.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let mut peer = accept(&server);
            let mut request = Vec::new();
            peer.read_to_end(&mut request).unwrap();
            assert_eq!(request, b"request");
            peer.write_all(b"complete response").unwrap();
        });
        let tunnel = Tunnel::open(Target::Local(worker_address)).unwrap();
        let mut peer = client(&tunnel);
        peer.write_all(b"request").unwrap();
        peer.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = Vec::new();
        peer.read_to_end(&mut response).unwrap();
        assert_eq!(response, b"complete response");
        worker.join().unwrap();
    }

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
