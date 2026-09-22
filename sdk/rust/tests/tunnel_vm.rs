use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use smolmachines::{ConnectOptions, Machine, Port};

#[test]
#[ignore = "requires a VM runtime or SMOL_CLOUD_TOKEN and SMOL_CLOUD_URL"]
fn published_service_survives_tunnel_reconnect_and_close() {
    let cloud = std::env::var("SMOL_CLOUD_TOKEN").ok();
    let options = match cloud.as_ref() {
        Some(key) => {
            ConnectOptions::with_api_key(key).base_url(std::env::var("SMOL_CLOUD_URL").unwrap())
        }
        None => ConnectOptions::default(),
    };
    let machine = Machine::builder(format!("rust-tunnel-{}", std::process::id()))
        .image("alpine:3.20")
        .cpus(1)
        .memory_mib(512)
        .network(true)
        .port(Port::new(18987, 8080))
        .wait_for_ports(false)
        .create_with(&options)
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if cloud.is_none() {
            machine.start().unwrap();
        }
        let exec = machine.exec(["sh", "-c", "apk add --no-cache busybox-extras && mkdir -p /workspace/web && printf rust-tunnel-ok > /workspace/web/index.html && busybox-extras httpd -p 8080 -h /workspace/web"]).unwrap();
        assert!(exec.success(), "{}", exec.stderr_utf8());
        for _ in 0..2 {
            let tunnel = machine.tunnel(8080).unwrap();
            let mut socket = TcpStream::connect(tunnel.address()).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            socket
                .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
                .unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).unwrap();
            assert!(response.contains("rust-tunnel-ok"), "{response}");
        }
        assert!(machine
            .exec([
                "sh",
                "-c",
                "wget -qO- http://127.0.0.1:8080 | grep -q rust-tunnel-ok"
            ])
            .unwrap()
            .success());
    }));
    machine.delete().unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
