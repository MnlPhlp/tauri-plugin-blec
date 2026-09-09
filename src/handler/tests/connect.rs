use std::sync::atomic::Ordering;
use std::time::Duration;

use super::helpers::*;
use crate::error::Error;
use crate::mock::{ConnectBehaviour, FailKind, Op, OpBehaviour};
use crate::models::{ScanFilter, WriteType};

#[tokio::test(start_paused = true)]
async fn connect_happy_path() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let mut updates = connection_updates(handler).await;

    let counter = connect_ok(handler, &device).await;

    assert_eq!(
        device.ops(),
        vec![Op::Connect, Op::DiscoverServices],
        "unexpected calls on the device"
    );
    assert_eq!(drain(&mut updates), vec![true]);
    let connected = handler.connected_device().await.unwrap();
    assert_eq!(connected.address, device.address_string());
    assert!(connected.is_connected);
    assert_eq!(handler.mtu().await.unwrap(), 247);

    // characteristics of every service are usable
    handler
        .send_data(CHARAC, None, &[1, 2, 3], WriteType::WithResponse)
        .await
        .unwrap();
    handler
        .send_data(CHARAC, Some(SERVICE2), &[4, 5], WriteType::WithoutResponse)
        .await
        .unwrap();
    assert_eq!(device.value(SERVICE, CHARAC), Some(vec![1, 2, 3]));
    assert_eq!(device.value(SERVICE2, CHARAC), Some(vec![4, 5]));
    assert_eq!(
        handler.recv_data(CHARAC, Some(SERVICE2)).await.unwrap(),
        vec![4, 5]
    );
    assert!(matches!(
        handler.recv_data(CHARAC_MISSING, None).await,
        Err(Error::CharacNotAvailable(_))
    ));

    assert_eq!(counter.load(Ordering::SeqCst), 0);
    assert!(handler.is_connected());
}

/// `connect` scans by itself when it does not know the address yet, and gives
/// up on that scan as soon as the device shows up instead of sitting out the
/// whole `ADDRESS_SCAN_MS`.
#[tokio::test(start_paused = true)]
async fn connect_without_prior_scan() {
    let (world, handler) = test_world();
    let device = stress_device(&world);

    let start = tokio::time::Instant::now();
    let (on_disconnect, _) = disconnect_counter();
    handler
        .connect(&device.address_string(), on_disconnect, false)
        .await
        .expect("connect without prior scan");
    assert!(handler.is_connected());
    assert!(device.is_connected());
    assert!(!handler.is_scanning().await, "scan left running");
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "connect sat out the whole scan: {:?}",
        start.elapsed()
    );
}

/// A device the app connected to once has to stay connectable. CoreBluetooth
/// drops its record of a peripheral when the link goes down while the stale
/// handle keeps showing up in `peripherals()`, so a connect that trusts the
/// device index fails with "Peripheral no longer available" until a new
/// advertisement happens to arrive.
#[tokio::test(start_paused = true)]
async fn reconnect_after_the_backend_forgot_the_peripheral() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    device.set_forget_on_disconnect(true);

    connect_ok(handler, &device).await;
    handler.disconnect().await.expect("disconnect failed");
    settle().await;
    assert!(!device.is_known(), "the backend kept the peripheral");

    // no scan in between: the address is still in the device index, only the
    // backend's record of the peripheral is gone
    let (on_disconnect, _) = disconnect_counter();
    handler
        .connect(&device.address_string(), on_disconnect, false)
        .await
        .expect("reconnect after the backend forgot the peripheral");
    assert!(handler.is_connected());
    assert!(device.is_connected());
}

/// `discover_services` resolves the peripheral the same way, so it copes with a
/// forgotten peripheral too.
#[tokio::test(start_paused = true)]
async fn discover_services_after_the_backend_forgot_the_peripheral() {
    let (world, handler) = test_world();
    let device = stress_device(&world);

    scan(handler, ScanFilter::None).await.expect("scan failed");
    device.forget();

    let services = handler
        .discover_services(&device.address_string())
        .await
        .expect("discover_services failed");
    assert_eq!(services.len(), 2, "{services:?}");
}

/// The same scan-if-unknown path, but for a device that never shows up:
/// `connect` must give up once the short scan is over instead of waiting
/// on the discovery channel forever.
#[tokio::test(start_paused = true)]
async fn connect_without_prior_scan_missing_device_does_not_hang() {
    let (world, handler) = test_world();
    // another device is around, so the scan does report devices - just not the wanted one
    let _other = other_device(&world);

    let (on_disconnect, counter) = disconnect_counter();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        handler.connect("11:22:33:44:55:66", on_disconnect, false),
    )
    .await
    .expect("connect did not return");
    assert!(
        matches!(result, Err(Error::UnknownPeripheral(_))),
        "{result:?}"
    );
    assert!(!handler.is_scanning().await, "scan left running");
    assert_disconnected(handler).await;
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn connect_unknown_address_fails_cleanly() {
    let (world, handler) = test_world();
    let _device = stress_device(&world);

    let (on_disconnect, counter) = disconnect_counter();
    let result = handler
        .connect("11:22:33:44:55:66", on_disconnect, false)
        .await;
    assert!(
        matches!(result, Err(Error::UnknownPeripheral(_))),
        "{result:?}"
    );
    assert_disconnected(handler).await;
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn connect_succeeds_after_retries() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    device.fail_next_connects(2, FailKind::NotConnected);

    let counter = connect_ok(handler, &device).await;
    assert_eq!(device.count_ops(&Op::Connect), 3);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn connect_gives_up_after_three_failures() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let mut updates = connection_updates(handler).await;
    device.set_connect_behaviour(ConnectBehaviour::Fail(FailKind::NotConnected));

    let (result, counter) = connect(handler, &device).await;
    assert!(matches!(result, Err(Error::Btleplug(_))), "{result:?}");
    assert_eq!(device.count_ops(&Op::Connect), 3);
    // a pending connectGatt is cancelled
    assert_eq!(device.ops().last(), Some(&Op::Disconnect));
    assert_disconnected(handler).await;
    assert!(!device.is_connected());
    // never connected: the callback must not run, but listeners hear "false"
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "disconnect callback ran for a failed connect"
    );
    assert_eq!(drain(&mut updates), vec![false]);

    // the next connect works again
    device.set_connect_behaviour(ConnectBehaviour::Ok);
    connect_ok(handler, &device).await;
}

#[tokio::test(start_paused = true)]
async fn connect_call_times_out() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    // longer than the connect timeout: `connect()` itself times out
    device.set_connect_behaviour(ConnectBehaviour::OkAfter(Duration::from_secs(10)));

    let (result, counter) = connect(handler, &device).await;
    assert!(matches!(result, Err(Error::Timeout(_))), "{result:?}");
    assert_eq!(device.count_ops(&Op::Connect), 3);
    assert_disconnected(handler).await;
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn connect_event_never_arrives() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    device.set_connect_behaviour(ConnectBehaviour::Hang);

    let (result, counter) = connect(handler, &device).await;
    assert!(matches!(result, Err(Error::Timeout(_))), "{result:?}");
    // every attempt cancelled its pending connection
    assert_eq!(device.count_ops(&Op::Connect), 3);
    assert!(device.count_ops(&Op::Disconnect) >= 3, "{:?}", device.ops());
    assert_disconnected(handler).await;
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    // the platform finishes the connection after the handler gave up
    device.clear_ops();
    device.complete_connect();
    assert!(
        wait_until(Duration::from_secs(1), || !device.is_connected()).await,
        "late connection was not hung up: {:?}",
        device.ops()
    );
    assert_eq!(device.ops(), vec![Op::Disconnect]);
    assert_disconnected(handler).await;
}

#[tokio::test(start_paused = true)]
async fn service_discovery_failure_tears_down_link() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let mut updates = connection_updates(handler).await;
    device.set_discover_behaviour(OpBehaviour::Fail(FailKind::Runtime("gatt".into())));

    let (result, counter) = connect(handler, &device).await;
    assert!(matches!(result, Err(Error::Btleplug(_))), "{result:?}");
    settle().await;
    assert!(
        !device.is_connected(),
        "link left open after failed discovery"
    );
    assert_disconnected(handler).await;
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "callback of a failed connect ran"
    );
    assert!(!drain(&mut updates).contains(&true), "reported connected");

    device.set_discover_behaviour(OpBehaviour::Ok);
    connect_ok(handler, &device).await;
}

#[tokio::test(start_paused = true)]
async fn service_discovery_timeout_tears_down_link() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    device.set_discover_behaviour(OpBehaviour::Hang);

    let (result, _) = connect(handler, &device).await;
    assert!(matches!(result, Err(Error::Timeout(_))), "{result:?}");
    settle().await;
    assert!(
        !device.is_connected(),
        "link left open after discovery timeout"
    );
    assert_disconnected(handler).await;
}

#[tokio::test(start_paused = true)]
async fn connect_to_second_device_disconnects_first() {
    let (world, handler) = test_world();
    let first = stress_device(&world);
    let second = world.add_device(
        crate::mock::DeviceSpec::new(OTHER_ADDR)
            .name("second")
            .service(crate::mock::ServiceSpec::new(SERVICE).characteristic(
                CHARAC,
                btleplug::api::CharPropFlags::READ | btleplug::api::CharPropFlags::WRITE,
            )),
    );
    let mut updates = connection_updates(handler).await;

    let first_counter = connect_ok(handler, &first).await;
    assert_eq!(drain(&mut updates), vec![true]);
    let second_counter = connect_ok(handler, &second).await;

    assert!(!first.is_connected(), "first device still connected");
    assert!(second.is_connected());
    assert!(first.ops().contains(&Op::Disconnect));
    assert_eq!(
        first_counter.load(Ordering::SeqCst),
        1,
        "callback of the first device"
    );
    assert_eq!(second_counter.load(Ordering::SeqCst), 0);
    assert_eq!(drain(&mut updates), vec![false, true]);
    assert_eq!(
        handler.connected_device().await.unwrap().address,
        second.address_string()
    );
}

#[tokio::test(start_paused = true)]
async fn connect_same_device_twice_is_a_noop() {
    let (world, handler) = test_world();
    let device = stress_device(&world);

    let first_counter = connect_ok(handler, &device).await;
    device.clear_ops();
    let second_counter = connect_ok(handler, &device).await;

    assert!(
        !device.ops().contains(&Op::Disconnect),
        "{:?}",
        device.ops()
    );
    assert_eq!(first_counter.load(Ordering::SeqCst), 0);
    assert_eq!(second_counter.load(Ordering::SeqCst), 0);
    assert!(device.is_connected());
}

#[tokio::test(start_paused = true)]
async fn connect_out_of_range_device() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    scan(handler, ScanFilter::None).await.unwrap();
    device.set_in_range(false);

    let (result, counter) = connect(handler, &device).await;
    assert!(result.is_err());
    assert_disconnected(handler).await;
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    device.set_in_range(true);
    connect_ok(handler, &device).await;
}

#[tokio::test(start_paused = true)]
async fn discover_services_without_connection() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    scan(handler, ScanFilter::None).await.unwrap();

    let services = handler
        .discover_services(&device.address_string())
        .await
        .unwrap();
    assert_eq!(services.len(), 2);
    let uuids: Vec<_> = services.iter().map(|s| s.uuid).collect();
    assert!(uuids.contains(&SERVICE) && uuids.contains(&SERVICE2));
    // it disconnects again afterwards
    assert!(
        wait_until(Duration::from_secs(2), || !device.is_connected()).await,
        "still connected after discovery"
    );
    assert_disconnected(handler).await;
}

#[tokio::test(start_paused = true)]
async fn discover_services_while_connected_keeps_connection() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    connect_ok(handler, &device).await;

    let services = handler
        .discover_services(&device.address_string())
        .await
        .unwrap();
    assert_eq!(services.len(), 2);
    settle().await;
    assert!(device.is_connected());
    assert!(handler.is_connected());
}

#[tokio::test(start_paused = true)]
async fn write_and_read_errors_are_reported() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    connect_ok(handler, &device).await;

    device.set_write_behaviour(OpBehaviour::Fail(FailKind::NotConnected));
    assert!(matches!(
        handler
            .send_data(CHARAC, None, &[1], WriteType::WithResponse)
            .await,
        Err(Error::Btleplug(_))
    ));
    device.set_write_behaviour(OpBehaviour::Hang);
    assert!(matches!(
        handler
            .send_data(CHARAC, None, &[1], WriteType::WithResponse)
            .await,
        Err(Error::Timeout(_))
    ));
    device.set_read_behaviour(OpBehaviour::Hang);
    assert!(matches!(
        handler.recv_data(CHARAC, None).await,
        Err(Error::Timeout(_))
    ));
    // the connection itself is unaffected by failed operations
    assert!(handler.is_connected());
    device.set_write_behaviour(OpBehaviour::Ok);
    handler
        .send_data(CHARAC, None, &[1], WriteType::WithResponse)
        .await
        .unwrap();
}
