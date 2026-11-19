//! Reuse the SDK transport in a caller-owned async gateway without allocating
//! a loopback listener or a runtime thread for each app.
use super::*;
use std::{
    fmt, io,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// An immutable local or authenticated cloud destination.
///
/// Creating this value opens no listener and starts no machine. The caller
/// controls route replacement and must drop old streams when access is revoked.
#[derive(Clone)]
pub struct TunnelTarget {
    pub(super) target: Target,
}
impl fmt::Debug for TunnelTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TunnelTarget")
            .field(
                "location",
                &match self.target {
                    Target::Local(_) => "local",
                    Target::Cloud(_) => "cloud",
                },
            )
            .finish_non_exhaustive()
    }
}
impl TunnelTarget {
    /// Route to an already verified published port on this computer.
    pub fn local(address: SocketAddr) -> Result<Self> {
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(Error::new(ErrorKind::Config, "Choose a loopback app port"));
        }
        Ok(Self {
            target: Target::Local(address),
        })
    }

    /// Prepare a cloud route with the client's selected account and credentials.
    /// Credential renewal can block; call this outside an async runtime or on
    /// its blocking pool. Recreate the target after credentials change.
    pub fn cloud(
        client: &smol_cloud::blocking::Client,
        machine_id: &str,
        port: u16,
    ) -> Result<Self> {
        Ok(Self {
            target: Target::from_client(client, machine_id, port)?,
        })
    }

    /// Open one byte stream on the caller's Tokio runtime. A cloud connection
    /// uses one bounded relay task; dropping the stream cancels that task and
    /// disconnects its authenticated transport.
    pub async fn connect(&self) -> Result<TunnelStream> {
        match &self.target {
            Target::Local(address) => {
                let remote = tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::net::TcpStream::connect(address),
                )
                .await
                .map_err(|_| connection_error())?
                .map_err(|_| connection_error())?;
                remote.set_nodelay(true).map_err(|_| connection_error())?;
                Ok(TunnelStream {
                    io: Box::new(remote),
                    relay: None,
                    failed: None,
                })
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
                    request.clone(),
                    Some(config),
                    false,
                    Some(tokio_tungstenite::Connector::Rustls(Arc::new(tls))),
                );
                let (ws, _) = tokio::time::timeout(Duration::from_secs(10), connect)
                    .await
                    .map_err(|_| connection_error())?
                    .map_err(|_| connection_error())?;
                let (stream, bridge) = tokio::io::duplex(65536);
                let failed = Arc::new(AtomicBool::new(false));
                let report = failed.clone();
                let relay = tokio::spawn(async move {
                    let (mut sink, mut stream) = ws.split();
                    let (mut reader, mut writer) = tokio::io::split(bridge);
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
                    let result: Result<()> =
                        tokio::select! { result = upload => result, result = download => result };
                    report.store(result.is_err(), Ordering::Release);
                });
                Ok(TunnelStream {
                    io: Box::new(stream),
                    relay: Some(relay),
                    failed: Some(failed),
                })
            }
        }
    }
}

trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// A connected stream whose transport work ends when this handle is dropped.
pub struct TunnelStream {
    io: Box<dyn Io>,
    relay: Option<tokio::task::JoinHandle<()>>,
    failed: Option<Arc<AtomicBool>>,
}
impl Drop for TunnelStream {
    fn drop(&mut self) {
        if let Some(relay) = self.relay.take() {
            relay.abort();
        }
    }
}
impl AsyncRead for TunnelStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let filled = buffer.filled().len();
        let result = Pin::new(&mut *self.io).poll_read(cx, buffer);
        if matches!(result, Poll::Ready(Ok(())))
            && buffer.filled().len() == filled
            && buffer.remaining() != 0
            && self
                .failed
                .as_ref()
                .is_some_and(|failed| failed.load(Ordering::Acquire))
        {
            Poll::Ready(Err(io::Error::other(connection_error())))
        } else {
            result
        }
    }
}
impl AsyncWrite for TunnelStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.io).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::result_large_err)] // tungstenite fixes the handshake callback's error type.
    fn cloud_stream_keeps_account_scope_streams_bounded_data_and_disconnects_on_drop() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = smol_cloud::blocking::Client::new(smol_cloud::Credentials::new(
            format!("http://{}", listener.local_addr().unwrap()),
            "owned-test-token",
        ))
        .unwrap()
        .with_tenant("owned-team")
        .unwrap();
        let target = TunnelTarget::cloud(&client, "owned-machine", 8080).unwrap();
        assert!(!format!("{target:?}").contains("owned-test-token"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_hdr_async(
                    stream,
                    |request: &Request<()>, response| {
                        assert_eq!(
                            request.uri().path(),
                            "/v1/machines/owned-machine/tunnel/8080"
                        );
                        assert_eq!(
                            request.headers()["authorization"],
                            "Bearer owned-test-token"
                        );
                        assert_eq!(request.headers()["smol-tenant"], "owned-team");
                        Ok(response)
                    },
                )
                .await
                .unwrap();
                let mut transferred = 0;
                while let Some(message) = socket.next().await {
                    match message {
                        Ok(Message::Binary(bytes)) => {
                            transferred += bytes.len();
                            socket.send(Message::Binary(bytes)).await.unwrap();
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        _ => {}
                    }
                }
                transferred
            });
            let stream = target.connect().await.unwrap();
            let (mut reader, mut writer) = tokio::io::split(stream);
            let payload: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
            let mut received = vec![0; payload.len()];
            tokio::time::timeout(Duration::from_secs(5), async {
                let (sent, read) =
                    tokio::join!(writer.write_all(&payload), reader.read_exact(&mut received));
                sent.unwrap();
                read.unwrap();
            })
            .await
            .unwrap();
            assert_eq!(received, payload);
            drop(reader);
            drop(writer);
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), server)
                    .await
                    .unwrap()
                    .unwrap(),
                payload.len()
            );
        });
    }

    #[test]
    fn malformed_cloud_frames_surface_a_stream_error_without_disclosing_response_contents() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = smol_cloud::blocking::Client::new(smol_cloud::Credentials::new(
            format!("http://{}", listener.local_addr().unwrap()),
            "owned-test-token",
        ))
        .unwrap();
        let target = TunnelTarget::cloud(&client, "owned-machine", 8080).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                socket
                    .send(Message::Text("private-cloud-detail".into()))
                    .await
                    .unwrap();
                let _ = socket.next().await;
            });
            let mut stream = target.connect().await.unwrap();
            let error = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut [0u8; 16]))
                .await
                .unwrap()
                .unwrap_err();
            assert!(!error.to_string().contains("private-cloud-detail"));
            drop(stream);
            tokio::time::timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap();
        });
    }
}
