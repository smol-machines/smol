//! Live check against a real engine: connecting to a machine that exists but
//! is stopped fails at once, and starts nothing. Run with:
//!   SMOLMACHINES_LIVE_IMAGE=alpine:3.20 cargo test --test live_connect_stopped -- --ignored --nocapture
use std::time::{Duration, Instant};

use smolmachines::{ConnectOptions, ErrorKind, Machine, MachineState};

#[test]
#[ignore]
fn connecting_to_a_stopped_machine_fails_fast_unless_asked_to_start_it() {
    let Some(image) = std::env::var("SMOLMACHINES_LIVE_IMAGE").ok() else {
        return;
    };
    let name = format!("sdk-live-stopped-{}", std::process::id());
    let created = Machine::builder(&name)
        .image(image)
        .cpus(1)
        .memory_mib(256)
        .create()
        .expect("create");
    assert_eq!(created.state(), MachineState::Stopped);

    let began = Instant::now();
    let error = Machine::connect(&name).expect_err("a stopped machine is not connected to");
    let took = began.elapsed();
    assert_eq!(error.kind(), ErrorKind::InvalidState, "{error}");
    let message = error.to_string();
    assert!(message.contains("is stopped"), "{message}");
    assert!(
        message.contains(&format!("smol machine start --name {name}")),
        "{message}"
    );
    assert!(
        took < Duration::from_secs(10),
        "the refusal took {took:?}; a start was attempted"
    );
    assert_eq!(
        Machine::attach(&name).expect("attach").state(),
        MachineState::Stopped,
        "connecting left the machine as it was"
    );

    let machine = Machine::connect_with(&name, &ConnectOptions::local().start_if_stopped())
        .expect("an opted-in connect starts the machine");
    machine.wait_until_ready().expect("ready");
    assert_eq!(
        Machine::connect(&name)
            .expect("a running machine connects")
            .state(),
        machine.state()
    );

    machine.stop().expect("stop");
    machine.delete().expect("delete");
}
