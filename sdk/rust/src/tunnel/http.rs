//! HTTP framing and browser-origin checks for private app addresses. All
//! forwarding futures belong to the accepted connection: cancelling a route
//! drops HTTP bodies, upstream transports and upgraded sockets together.
use super::{connection_error, relay, Target};
use crate::Result;
use futures_util::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::{
    body::{Bytes, Incoming},
    header::{self, HeaderMap, HeaderValue},
    service::service_fn,
    Method, Request, Response, StatusCode, Version,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{convert::Infallible, net::SocketAddr, time::Duration};
use tokio::{net::TcpStream, sync::mpsc};

type Body = UnsyncBoxBody<Bytes, hyper::Error>;
type Task = BoxFuture<'static, Result<()>>;

pub(super) async fn serve(socket: TcpStream, target: Target, address: SocketAddr) -> Result<()> {
    let (tasks, mut pending) = mpsc::channel::<Task>(8);
    let service = service_fn(move |request| {
        let target = target.clone();
        let tasks = tasks.clone();
        async move {
            Ok::<_, Infallible>(match forward(request, target, address, tasks).await {
                Ok(response) => response,
                Err(_) => reply(StatusCode::BAD_GATEWAY, "App connection failed.\n"),
            })
        }
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10))
        .max_headers(100)
        .max_buf_size(32768);
    let connection = builder
        .serve_connection(TokioIo::new(socket), service)
        .with_upgrades();
    tokio::pin!(connection);
    let mut active = FuturesUnordered::new();
    let mut finished = false;
    loop {
        if finished && active.is_empty() && pending.is_empty() {
            return Ok(());
        }
        tokio::select! {
            result = &mut connection, if !finished => {
                result.map_err(|_| connection_error())?;
                finished = true;
            }
            Some(task) = pending.recv(), if active.len() < 16 => active.push(task),
            Some(_) = active.next(), if !active.is_empty() => {},
        }
    }
}

fn reply(status: StatusCode, text: &'static str) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from_static(text.as_bytes()))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn single_header(headers: &HeaderMap, name: &str, expected: &str, required: bool) -> bool {
    let mut values = headers.get_all(name).iter();
    let valid = match values.next() {
        None => !required,
        Some(value) => value.as_bytes() == expected.as_bytes(),
    };
    valid && values.next().is_none()
}

fn allowed<B>(request: &Request<B>, address: SocketAddr) -> bool {
    let authority = address.to_string();
    let origin = format!("http://{authority}");
    request.version() == Version::HTTP_11
        && request.method() != Method::CONNECT
        && request.uri().scheme().is_none()
        && request.uri().authority().is_none()
        && request.uri().path().starts_with('/')
        && single_header(request.headers(), "host", &authority, true)
        && single_header(request.headers(), "origin", &origin, false)
        && (single_header(request.headers(), "sec-fetch-site", "same-origin", false)
            || single_header(request.headers(), "sec-fetch-site", "none", false))
}

fn websocket(headers: &HeaderMap) -> bool {
    single_header(headers, "upgrade", "websocket", true)
        && headers.get_all(header::CONNECTION).iter().any(|value| {
            value.to_str().is_ok_and(|value| {
                value
                    .split(',')
                    .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
            })
        })
}

fn strip_hop_headers(headers: &mut HeaderMap, upgrade: bool) {
    let names: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|v| header::HeaderName::from_bytes(v.trim().as_bytes()).ok())
        .collect();
    for name in names {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
    if upgrade {
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    }
}

async fn forward(
    mut request: Request<Incoming>,
    target: Target,
    address: SocketAddr,
    tasks: mpsc::Sender<Task>,
) -> Result<Response<Body>> {
    // Check every request, including keep-alive/pipelined requests, before
    // creating any upstream connection or presenting cloud credentials.
    if !allowed(&request, address) {
        return Ok(reply(StatusCode::FORBIDDEN, "Use this app's local URL.\n"));
    }
    let upgrade = websocket(request.headers());
    if request.headers().contains_key(header::UPGRADE)
        && (!upgrade || request.method() != Method::GET)
    {
        return Ok(reply(
            StatusCode::BAD_REQUEST,
            "Unsupported app protocol.\n",
        ));
    }
    let browser = upgrade.then(|| hyper::upgrade::on(&mut request));
    strip_hop_headers(request.headers_mut(), upgrade);
    // Connection-nominated headers cannot change the validated authority.
    request.headers_mut().insert(
        header::HOST,
        address
            .to_string()
            .parse()
            .map_err(|_| connection_error())?,
    );
    let (client, upstream) = tokio::io::duplex(65536);
    tasks
        .try_send(relay(upstream, target).boxed())
        .map_err(|_| connection_error())?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(client))
        .await
        .map_err(|_| connection_error())?;
    tasks
        .try_send(
            async move {
                connection
                    .with_upgrades()
                    .await
                    .map_err(|_| connection_error())
            }
            .boxed(),
        )
        .map_err(|_| connection_error())?;
    let mut response = sender
        .send_request(request)
        .await
        .map_err(|_| connection_error())?;
    let upgraded = response.status() == StatusCode::SWITCHING_PROTOCOLS;
    if upgraded {
        let browser = browser.ok_or_else(connection_error)?;
        if !websocket(response.headers()) {
            return Err(connection_error());
        }
        let upstream = hyper::upgrade::on(&mut response);
        tasks
            .try_send(
                async move {
                    let (browser, upstream) =
                        tokio::try_join!(browser, upstream).map_err(|_| connection_error())?;
                    tokio::io::copy_bidirectional(
                        &mut TokioIo::new(browser),
                        &mut TokioIo::new(upstream),
                    )
                    .await
                    .map_err(|_| connection_error())?;
                    Ok(())
                }
                .boxed(),
            )
            .map_err(|_| connection_error())?;
    }
    strip_hop_headers(response.headers_mut(), upgraded);
    Ok(response.map(BodyExt::boxed_unsync))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tunnel;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream as StdStream},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        thread,
    };

    struct App {
        address: SocketAddr,
        requests: Arc<AtomicUsize>,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl App {
        fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            listener.set_nonblocking(true).unwrap();
            let requests = Arc::new(AtomicUsize::new(0));
            let count = requests.clone();
            let (stop, mut stopped) = tokio::sync::oneshot::channel();
            let worker = thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async move {
                        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                        let mut clients = tokio::task::JoinSet::new();
                        loop {
                            tokio::select! {
                                _ = &mut stopped => break,
                                Some(result) = clients.join_next(), if !clients.is_empty() => {
                                    result.unwrap();
                                }
                                accepted = listener.accept() => {
                                    let (socket, _) = accepted.unwrap();
                                    let count = count.clone();
                                    clients.spawn(async move {
                                        hyper::server::conn::http1::Builder::new()
                                            .serve_connection(TokioIo::new(socket), service_fn(move |request: Request<Incoming>| {
                                                count.fetch_add(1, Ordering::SeqCst);
                                                async move {
                                                    let response = if request.uri().path() == "/echo" {
                                                        Response::new(request.into_body().boxed_unsync())
                                                    } else {
                                                        reply(StatusCode::OK, "local app\n")
                                                    };
                                                    Ok::<_, Infallible>(response)
                                                }
                                            }))
                                            .await.ok();
                                    });
                                }
                            }
                        }
                        clients.abort_all();
                        while clients.join_next().await.is_some() {}
                    });
            });
            Self {
                address,
                requests,
                stop: Some(stop),
                worker: Some(worker),
            }
        }
    }

    impl Drop for App {
        fn drop(&mut self) {
            let _ = self.stop.take().unwrap().send(());
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    fn connect(address: SocketAddr) -> StdStream {
        let socket = StdStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
    }

    fn read_response(socket: &mut StdStream) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            assert!(bytes.len() < 32768);
        }
        let headers = String::from_utf8(bytes).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        let mut body = vec![0; length];
        socket.read_exact(&mut body).unwrap();
        headers + &String::from_utf8(body).unwrap()
    }

    #[test]
    fn rejects_browser_origin_attacks_before_opening_upstream() {
        let app = App::new();
        let mut tunnel = Tunnel::bind_http(0).unwrap();
        tunnel.route_local(app.address).unwrap();
        let url = format!("http://{}/", tunnel.address());
        let client = client();
        for (name, value) in [
            ("Host", "rebound.example"),
            ("Host", "localhost"),
            ("Origin", "https://attacker.example"),
            ("Origin", "null"),
            ("Sec-Fetch-Site", "cross-site"),
            ("Sec-Fetch-Site", "same-site"),
        ] {
            assert_eq!(
                client
                    .get(&url)
                    .header(name, value)
                    .send()
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        for target in ["http://attacker.example/", "*"] {
            let mut socket = connect(tunnel.address());
            write!(
                socket,
                "GET {target} HTTP/1.1\r\nHost: {}\r\n\r\n",
                tunnel.address()
            )
            .unwrap();
            assert!(read_response(&mut socket).starts_with("HTTP/1.1 403"));
        }
        let mut socket = connect(tunnel.address());
        write!(
            socket,
            "CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n",
            app.address,
            tunnel.address()
        )
        .unwrap();
        assert!(read_response(&mut socket).starts_with("HTTP/1.1 403"));
        assert_eq!(app.requests.load(Ordering::SeqCst), 0);
        assert_eq!(
            client
                .get(&url)
                .header("Origin", url.trim_end_matches('/'))
                .header("Sec-Fetch-Site", "same-origin")
                .send()
                .unwrap()
                .text()
                .unwrap(),
            "local app\n"
        );
        assert_eq!(app.requests.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn validates_each_keep_alive_request_and_duplicate_origins() {
        let app = App::new();
        let mut tunnel = Tunnel::bind_http(0).unwrap();
        tunnel.route_local(app.address).unwrap();
        let address = tunnel.address();
        let mut socket = connect(address);
        write!(socket, "GET / HTTP/1.1\r\nHost: {address}\r\n\r\n").unwrap();
        assert!(read_response(&mut socket).ends_with("local app\n"));
        // Both requests arrive on the already validated TCP connection.
        write!(socket, "GET / HTTP/1.1\r\nHost: attacker.example\r\n\r\nGET / HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\nOrigin: https://attacker.example\r\n\r\n").unwrap();
        assert!(read_response(&mut socket).starts_with("HTTP/1.1 403"));
        assert!(read_response(&mut socket).starts_with("HTTP/1.1 403"));
        assert_eq!(app.requests.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn streams_fixed_and_chunked_bodies_larger_than_transport_buffers() {
        let app = App::new();
        let mut tunnel = Tunnel::bind_http(0).unwrap();
        tunnel.route_local(app.address).unwrap();
        let url = format!("http://{}/echo", tunnel.address());
        let body: Vec<_> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
        let client = client();
        let fixed = client
            .post(&url)
            .body(body.clone())
            .send()
            .unwrap()
            .bytes()
            .unwrap();
        assert_eq!(fixed.as_ref(), body);
        let chunked = client
            .post(&url)
            .body(reqwest::blocking::Body::new(std::io::Cursor::new(
                body.clone(),
            )))
            .send()
            .unwrap()
            .bytes()
            .unwrap();
        assert_eq!(chunked.as_ref(), body);
    }

    #[test]
    fn websocket_echo_is_closed_by_suspension() {
        use tokio_tungstenite::tungstenite::{accept, client, Message};
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut tunnel = Tunnel::bind_http(0).unwrap();
        tunnel.route_local(server.local_addr().unwrap()).unwrap();
        let (closed, observed) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let (socket, _) = server.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut ws = accept(socket).unwrap();
            let message = ws.read().unwrap();
            ws.send(message).unwrap();
            let result = ws.read();
            let disconnected = match result {
                Err(
                    tokio_tungstenite::tungstenite::Error::Protocol(_)
                    | tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                ) => true,
                Err(tokio_tungstenite::tungstenite::Error::Io(error)) => matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::UnexpectedEof
                ),
                _ => false,
            };
            closed.send(disconnected).unwrap();
        });
        let (mut ws, _) = client(
            format!("ws://{}/socket", tunnel.address()),
            connect(tunnel.address()),
        )
        .unwrap();
        ws.send(Message::Text("hot reload".into())).unwrap();
        assert_eq!(ws.read().unwrap(), Message::Text("hot reload".into()));
        tunnel.suspend().unwrap();
        assert!(ws.read().is_err());
        assert!(observed.recv_timeout(Duration::from_secs(5)).unwrap());
        worker.join().unwrap();
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn browser_address_survives_local_cloud_return_without_exposing_credentials() {
        use tokio_tungstenite::tungstenite::{accept_hdr, Message};
        let app = App::new();
        let cloud = TcpListener::bind("127.0.0.1:0").unwrap();
        cloud.set_nonblocking(true).unwrap();
        let client_config = smol_cloud::blocking::Client::new(smol_cloud::Credentials::new(
            format!("http://{}", cloud.local_addr().unwrap()),
            "cloud-secret-token",
        ))
        .unwrap()
        .with_tenant("app-owner")
        .unwrap();
        let mut tunnel = Tunnel::bind_http(0).unwrap();
        let address = tunnel.address();
        let url = format!("http://{address}/");
        let browser = client();
        tunnel.route_local(app.address).unwrap();
        assert_eq!(
            browser.get(&url).send().unwrap().text().unwrap(),
            "local app\n"
        );
        tunnel.suspend().unwrap();
        tunnel
            .route_cloud(&client_config, "machine-original", 3000)
            .unwrap();
        assert_eq!(
            browser
                .get(&url)
                .header("Host", "attacker.example")
                .send()
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        // Rejected browser traffic never connects to the authenticated bridge.
        assert_eq!(
            cloud.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        cloud.set_nonblocking(false).unwrap();
        let worker = thread::spawn(move || {
            let (socket, _) = cloud.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut ws = accept_hdr(socket, |request: &Request<()>, response| {
                assert_eq!(
                    request.uri().path(),
                    "/v1/machines/machine-original/tunnel/3000"
                );
                assert_eq!(
                    request.headers()["authorization"],
                    "Bearer cloud-secret-token"
                );
                assert_eq!(request.headers()["smol-tenant"], "app-owner");
                Ok(response)
            })
            .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let Message::Binary(data) = ws.read().unwrap() else {
                    panic!("expected HTTP bytes")
                };
                request.extend_from_slice(&data);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request
                .to_ascii_lowercase()
                .contains(&format!("host: {address}")));
            assert!(!request.contains("cloud-secret-token"));
            assert!(!request.to_ascii_lowercase().contains("smol-tenant"));
            ws.send(Message::Binary(
                b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\ncloud app"
                    .to_vec()
                    .into(),
            ))
            .unwrap();
            let _ = ws.close(None);
        });
        assert_eq!(
            browser.get(&url).send().unwrap().text().unwrap(),
            "cloud app"
        );
        worker.join().unwrap();
        tunnel.suspend().unwrap();
        tunnel.route_local(app.address).unwrap();
        assert_eq!(tunnel.address(), address);
        assert_eq!(
            browser.get(&url).send().unwrap().text().unwrap(),
            "local app\n"
        );
    }
}
