//! Randomised stress test: a seeded sequence of connects, disconnects, link
//! drops, disappearing devices, adapter power cycles, notification bursts and
//! spurious events, checking the handler's invariants after every step.
//!
//! Configure with `BLEC_STRESS_SEED` (default 1) and `BLEC_STRESS_ITERS`
//! (default 300), e.g. `BLEC_STRESS_ITERS=5000 cargo test stress -- --nocapture`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use btleplug::api::CentralState;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use super::helpers::*;
use crate::error::Error;
use crate::handler::{Handler, OnDisconnectHandler};
use crate::mock::{ConnectBehaviour, DeviceHandle, FailKind, OpBehaviour};
use crate::models::{ScanFilter, WriteType};

#[derive(Debug, Clone, Copy)]
enum Step {
    Connect,
    Disconnect,
    DropLink,
    DropLinkSilently,
    Hide,
    Show,
    PowerOff,
    PowerOn,
    NotifyBurst,
    SpuriousConnect,
    SpuriousDisconnect,
    WriteRead,
    Subscribe,
    FlakyConnects,
    SlowDiscovery,
    HangingConnect,
}

const STEPS: &[Step] = &[
    Step::Connect,
    Step::Connect,
    Step::Connect,
    Step::Disconnect,
    Step::Disconnect,
    Step::DropLink,
    Step::DropLink,
    Step::DropLinkSilently,
    Step::Hide,
    Step::Show,
    Step::PowerOff,
    Step::PowerOn,
    Step::NotifyBurst,
    Step::SpuriousConnect,
    Step::SpuriousDisconnect,
    Step::WriteRead,
    Step::WriteRead,
    Step::Subscribe,
    Step::FlakyConnects,
    Step::SlowDiscovery,
    Step::HangingConnect,
];

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Model {
    /// Connections the handler reported established (connect returned Ok)
    established: usize,
    /// Number of callbacks the handler must have run by now
    callbacks: Arc<AtomicUsize>,
    /// Whether the last established connection is still up in the handler's eyes
    handler_connected: bool,
    notified: Arc<Mutex<usize>>,
}

async fn connect_step(
    handler: &'static Handler,
    device: &DeviceHandle,
    model: &mut Model,
) -> Result<(), Error> {
    let callbacks = model.callbacks.clone();
    let result = handler
        .connect(
            &device.address_string(),
            OnDisconnectHandler::from_sync(move || {
                callbacks.fetch_add(1, Ordering::SeqCst);
            }),
            false,
        )
        .await;
    if result.is_ok() {
        // connecting to the already connected device reuses the connection
        if !model.handler_connected {
            model.established += 1;
        }
        model.handler_connected = true;
    }
    result
}

/// The invariants that must hold whenever no operation is in flight.
async fn check_invariants(handler: &Handler, device: &DeviceHandle, model: &mut Model, step: &str) {
    let connected = handler.is_connected();
    if connected {
        assert!(
            device.is_connected(),
            "[{step}] handler connected but device link is down"
        );
        assert!(
            handler.connected_device().await.is_ok(),
            "[{step}] connected but connected_device() fails"
        );
    } else {
        assert!(
            matches!(
                handler.connected_device().await,
                Err(Error::NoDeviceConnected)
            ),
            "[{step}] disconnected but connected_device() returns a device"
        );
        assert!(
            matches!(handler.mtu().await, Err(Error::NoDeviceConnected)),
            "[{step}] disconnected but mtu() works"
        );
        if model.handler_connected {
            // the connection we knew about ended: exactly one callback more
            model.handler_connected = false;
        }
    }
    let callbacks = model.callbacks.load(Ordering::SeqCst);
    let ended = model.established - usize::from(model.handler_connected);
    assert_eq!(
        callbacks, ended,
        "[{step}] disconnect callback ran {callbacks} times for {ended} ended connections"
    );
}

/// Waits until the handler is not in the middle of reacting to an event.
async fn quiesce(handler: &Handler, device: &DeviceHandle) {
    // the handler agrees with the device, or it can't find out (silent drop)
    let _ = wait_until(Duration::from_secs(3), || {
        handler.is_connected() == device.is_connected()
    })
    .await;
    settle().await;
}

#[tokio::test(start_paused = true)]
async fn randomized_connection_stress() {
    let seed = env_u64("BLEC_STRESS_SEED", 1);
    let iters = env_u64("BLEC_STRESS_ITERS", 300);
    eprintln!("stress test: BLEC_STRESS_SEED={seed} BLEC_STRESS_ITERS={iters}");
    let mut rng = StdRng::seed_from_u64(seed);

    let (world, handler) = test_world();
    let device = stress_device(&world);
    let other = other_device(&world);
    let mut model = Model {
        established: 0,
        callbacks: Arc::new(AtomicUsize::new(0)),
        handler_connected: false,
        notified: Arc::new(Mutex::new(0)),
    };
    let mut history: Vec<String> = vec![];
    // both devices must be known to the handler (see `connect_without_prior_scan`)
    scan(handler, ScanFilter::None).await.unwrap();

    for i in 0..iters {
        let step = STEPS[rng.gen_range(0..STEPS.len())];
        let label = format!("#{i} {step:?}");
        history.push(label.clone());
        match step {
            Step::Connect => {
                if world.adapter_state() == CentralState::PoweredOff {
                    world.set_adapter_state(CentralState::PoweredOn);
                }
                let _ = connect_step(handler, &device, &mut model).await;
            }
            Step::Disconnect => {
                let _ = handler.disconnect().await;
            }
            Step::DropLink => {
                device.drop_link();
            }
            Step::DropLinkSilently => {
                device.drop_link_silently();
                // the handler can only find out through an operation
                let _ = handler.recv_data(CHARAC, None).await;
                if handler.is_connected() && !device.is_connected() {
                    // the desync is repaired by disconnect()
                    let _ = handler.disconnect().await;
                }
            }
            Step::Hide => device.set_in_range(false),
            Step::Show => device.set_in_range(true),
            Step::PowerOff => world.set_adapter_state(CentralState::PoweredOff),
            Step::PowerOn => world.set_adapter_state(CentralState::PoweredOn),
            Step::NotifyBurst => {
                let n = rng.gen_range(1..200);
                for k in 0..n {
                    device.notify(SERVICE, CHARAC_NOTIFY, vec![k as u8]);
                }
            }
            Step::SpuriousConnect => other.complete_connect(),
            Step::SpuriousDisconnect => {
                other.emit_disconnected_event();
                device.emit_disconnected_event();
            }
            Step::WriteRead => {
                let data: Vec<u8> = (0..rng.gen_range(1..50)).map(|_| rng.gen()).collect();
                let write = handler
                    .send_data(CHARAC, Some(SERVICE), &data, WriteType::WithResponse)
                    .await;
                let read = handler.recv_data(CHARAC, Some(SERVICE)).await;
                if let (Ok(()), Ok(read)) = (write, read) {
                    assert_eq!(read, data, "[{label}] read back different data");
                }
            }
            Step::Subscribe => {
                let notified = model.notified.clone();
                let _ = handler
                    .subscribe(CHARAC_NOTIFY, None, move |_data| {
                        *notified.lock().unwrap() += 1;
                    })
                    .await;
            }
            Step::FlakyConnects => {
                let fails = rng.gen_range(1..4);
                device.fail_next_connects(fails, FailKind::NotConnected);
                let _ = connect_step(handler, &device, &mut model).await;
                // not consumed if the connect failed earlier (e.g. out of range)
                device.clear_connect_queue();
            }
            Step::SlowDiscovery => {
                device.set_discover_behaviour(OpBehaviour::Delay(Duration::from_millis(
                    rng.gen_range(0..1500),
                )));
                let _ = connect_step(handler, &device, &mut model).await;
                device.set_discover_behaviour(OpBehaviour::Ok);
            }
            Step::HangingConnect => {
                device.set_connect_behaviour(ConnectBehaviour::Hang);
                let _ = connect_step(handler, &device, &mut model).await;
                device.set_connect_behaviour(ConnectBehaviour::Ok);
                if rng.gen_bool(0.5) {
                    // the platform completes the connection late
                    device.complete_connect();
                }
            }
        }
        quiesce(handler, &device).await;
        // the spurious "other" link must never survive
        if other.is_connected() {
            assert!(
                wait_until(Duration::from_secs(1), || !other.is_connected()).await,
                "[{label}] unrequested link to the other device kept open"
            );
        }
        check_invariants(handler, &device, &mut model, &label).await;
    }

    // clean end: everything can still connect and disconnect
    device.set_in_range(true);
    if world.adapter_state() == CentralState::PoweredOff {
        world.set_adapter_state(CentralState::PoweredOn);
    }
    if handler.is_connected() {
        handler.disconnect().await.unwrap();
    }
    scan(handler, ScanFilter::None).await.unwrap();
    connect_step(handler, &device, &mut model)
        .await
        .unwrap_or_else(|e| panic!("final connect failed: {e}\nhistory: {history:#?}"));
    handler.disconnect().await.unwrap();
    quiesce(handler, &device).await;
    check_invariants(handler, &device, &mut model, "final").await;
    eprintln!(
        "stress test done: {} connections established, {} notifications received",
        model.established,
        *model.notified.lock().unwrap()
    );
}
