//! The application's scan next to connects: a connect pauses it and gives the
//! radio back, it never ends it.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::helpers::*;
use crate::error::Error;
use crate::handler::Handler;
use crate::mock::{ConnectBehaviour, DeviceHandle, FailKind, MockWorld, Op};
use crate::models::{BleDevice, ScanFilter};

/// How long the application's scan runs in these tests.
const SCAN_MS: u64 = 10_000;

/// Starts the application's scan, the way a device list does.
async fn app_scan(
    handler: &'static Handler,
    filter: ScanFilter,
) -> (mpsc::Receiver<Vec<BleDevice>>, Instant) {
    assert!(!handler.is_scanning().await);
    let (tx, rx) = mpsc::channel(256);
    let start = Instant::now();
    handler
        .discover(Some(tx), SCAN_MS, filter, false)
        .await
        .expect("discover failed");
    assert!(handler.is_scanning().await);
    (rx, start)
}

/// A device that shows up later: the backend has no record of it, so only an
/// adapter that really scans once it is in range gets it into `peripherals()`.
/// A scan that was not resumed never delivers it.
fn late_device(world: &MockWorld) -> DeviceHandle {
    late(other_device(world))
}

/// Makes `device` one that shows up later, see [`late_device`].
fn late(device: DeviceHandle) -> DeviceHandle {
    device.set_in_range(false);
    device.expire();
    device
}

/// Whether the scan delivers `device` within a second.
async fn delivers(rx: &mut mpsc::Receiver<Vec<BleDevice>>, device: &DeviceHandle) -> bool {
    let address = device.address_string();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        while let Ok(devices) = rx.try_recv() {
            if devices.iter().any(|d| d.address == address) {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// The scan is still the application's, delivers a device that only now comes
/// into range, and ends when the application asked for, with one `true` and
/// one `false` scan update over its whole life.
async fn assert_scan_goes_on(
    world: &MockWorld,
    handler: &'static Handler,
    mut rx: mpsc::Receiver<Vec<BleDevice>>,
    start: Instant,
    updates: &mut mpsc::Receiver<bool>,
    late: &DeviceHandle,
) {
    assert!(handler.is_scanning().await, "the scan is gone");
    assert!(world.is_scanning(), "the adapter does not scan any more");
    late.set_in_range(true);
    assert!(
        delivers(&mut rx, late).await,
        "a device that appeared after the connect was not delivered"
    );

    while handler.is_scanning().await {
        assert!(start.elapsed() < Duration::from_secs(60), "scan never ends");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let ran = start.elapsed();
    assert!(
        ran >= Duration::from_millis(SCAN_MS) && ran < Duration::from_millis(SCAN_MS + 500),
        "the scan ran {ran:?} instead of {SCAN_MS} ms"
    );
    settle().await;
    assert!(!world.is_scanning(), "adapter left scanning");
    assert_eq!(drain(updates), vec![true, false]);
    // the scan's task is gone, and its sender with it
    while rx.recv().await.is_some() {}
}

async fn connect_to(handler: &'static Handler, device: &DeviceHandle) -> Result<(), Error> {
    let (on_disconnect, _) = disconnect_counter();
    handler
        .connect(&device.address_string(), on_disconnect, false)
        .await
        .map(|_| ())
}

#[tokio::test(start_paused = true)]
async fn app_scan_survives_a_connect() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let late = late_device(&world);
    scan(handler, ScanFilter::None).await.expect("scan failed");
    device.set_connect_behaviour(ConnectBehaviour::OkAfter(Duration::from_millis(500)));
    let mut updates = scanning_updates(handler).await;

    let (rx, start) = app_scan(handler, ScanFilter::None).await;
    settle().await;
    assert!(world.is_scanning());
    let scans = world.scan_starts();

    let (result, ()) = tokio::join!(connect_to(handler, &device), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        // the radio is the connect's, the scan is still the application's
        assert!(!world.is_scanning(), "scanning while connecting");
        assert!(handler.is_scanning().await, "the scan ended");
    });
    result.expect("connect failed");
    assert!(device.is_connected());
    assert_eq!(world.scan_starts(), scans + 1, "resumed once");

    assert_scan_goes_on(&world, handler, rx, start, &mut updates, &late).await;
}

#[tokio::test(start_paused = true)]
async fn app_scan_survives_a_failed_connect() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let late = late_device(&world);
    scan(handler, ScanFilter::None).await.expect("scan failed");
    device.set_connect_behaviour(ConnectBehaviour::Fail(FailKind::TimedOut));
    let mut updates = scanning_updates(handler).await;

    let (rx, start) = app_scan(handler, ScanFilter::None).await;
    settle().await;
    let scans = world.scan_starts();

    let (result, ()) = tokio::join!(connect_to(handler, &device), async {
        // between the tries
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!world.is_scanning(), "scanning while connecting");
        assert!(handler.is_scanning().await, "the scan ended");
    });
    assert!(matches!(result, Err(Error::Btleplug(_))), "{result:?}");
    assert_eq!(device.count_ops(&Op::Connect), 3);
    assert_eq!(world.scan_starts(), scans + 1, "resumed once");

    assert_scan_goes_on(&world, handler, rx, start, &mut updates, &late).await;
}

/// A connect to an address the index does not have finds it through the
/// application's scan instead of replacing that scan with its own, although
/// the scan's filter keeps the device from the application.
#[tokio::test(start_paused = true)]
async fn connect_finds_its_address_through_the_app_scan() {
    let (world, handler) = test_world();
    // has `SERVICE` only: the filter hides it
    let device = second_device(&world);
    // has `SERVICE2`: the filter lets it through
    let late = late(stress_device(&world));
    let mut updates = scanning_updates(handler).await;

    let (mut rx, start) = app_scan(handler, ScanFilter::Service(SERVICE2)).await;
    settle().await;
    let scans = world.scan_starts();

    connect_to(handler, &device).await.expect("connect failed");
    assert!(device.is_connected());
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "the address scan did not end when the device showed up"
    );
    assert_eq!(
        world.scan_starts(),
        scans + 1,
        "only the scan paused for the connect was started again"
    );
    assert!(
        drain(&mut rx).is_empty(),
        "the filtered scan delivered the device"
    );

    // the scan goes on with its filter
    assert_scan_goes_on(&world, handler, rx, start, &mut updates, &late).await;
}

/// The rescan for a device the backend dropped runs on the application's scan
/// as well, and that scan goes on after it.
#[tokio::test(start_paused = true)]
async fn app_scan_survives_a_connect_that_rescans() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let late = late_device(&world);
    scan(handler, ScanFilter::None).await.expect("scan failed");
    // silent until the connect has the scan paused, so that the application's
    // scan does not bring it back before the connect fails on it
    device.set_advertising(false);
    device.expire();
    let mut updates = scanning_updates(handler).await;

    let (rx, start) = app_scan(handler, ScanFilter::None).await;
    settle().await;
    let scans = world.scan_starts();

    let (result, ()) = tokio::join!(connect_to(handler, &device), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!world.is_scanning(), "scanning while connecting");
        device.set_advertising(true);
    });
    result.expect("connect failed");
    assert_eq!(
        device.ops(),
        vec![Op::Connect, Op::Connect, Op::DiscoverServices]
    );
    assert_eq!(
        world.scan_starts(),
        scans + 2,
        "resumed for the rescan and after the connect, no scan of its own"
    );

    assert_scan_goes_on(&world, handler, rx, start, &mut updates, &late).await;
}

/// A scan without iBeacons cannot find an iBeacon: the connect scans on its
/// own next to the paused scan, which goes on afterwards.
#[tokio::test(start_paused = true)]
async fn app_scan_survives_an_address_scan_of_its_own() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let mut updates = scanning_updates(handler).await;

    // nothing passes the filter, so the index stays empty
    let (rx, start) = app_scan(handler, ScanFilter::Service(CHARAC_MISSING)).await;
    settle().await;
    let scans = world.scan_starts();

    let (on_disconnect, _) = disconnect_counter();
    handler
        .connect(&device.address_string(), on_disconnect, true)
        .await
        .expect("connect failed");
    assert!(device.is_connected());
    assert_eq!(
        world.scan_starts(),
        scans + 3,
        "the address scan, the scan resumed after it and after the connect"
    );
    assert!(handler.is_scanning().await, "the scan is gone");
    assert!(world.is_scanning(), "the adapter does not scan any more");

    // nothing is delivered through this filter; the scan still has to end on time
    while handler.is_scanning().await {
        assert!(start.elapsed() < Duration::from_secs(60), "scan never ends");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(start.elapsed() < Duration::from_millis(SCAN_MS + 500));
    settle().await;
    assert!(!world.is_scanning(), "adapter left scanning");
    assert_eq!(drain(&mut updates), vec![true, false]);
    drop(rx);
}

#[tokio::test(start_paused = true)]
async fn stop_scan_during_a_connect_ends_the_scan_for_good() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    scan(handler, ScanFilter::None).await.expect("scan failed");
    device.set_connect_behaviour(ConnectBehaviour::OkAfter(Duration::from_millis(500)));
    let mut updates = scanning_updates(handler).await;

    let (mut rx, _) = app_scan(handler, ScanFilter::None).await;
    settle().await;

    let (result, ()) = tokio::join!(connect_to(handler, &device), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(handler.is_scanning().await);
        handler.stop_scan().await.expect("stop_scan failed");
        assert!(!handler.is_scanning().await);
    });
    result.expect("connect failed");
    assert!(!handler.is_scanning().await, "the connect revived the scan");
    assert!(!world.is_scanning(), "the connect revived the adapter scan");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!handler.is_scanning().await);
    assert!(!world.is_scanning());
    assert_eq!(drain(&mut updates), vec![true, false]);
    // its channel is closed
    while rx.recv().await.is_some() {}
}

/// `stop_scan` while a connect looks for its address through the scan: the
/// application's scan is over, the connect keeps looking on its own and
/// leaves nothing running.
#[tokio::test(start_paused = true)]
async fn stop_scan_during_an_address_scan() {
    let (world, handler) = test_world();
    let _other = other_device(&world);
    let mut updates = scanning_updates(handler).await;

    let (mut rx, _) = app_scan(handler, ScanFilter::None).await;
    settle().await;

    let (on_disconnect, _) = disconnect_counter();
    let (result, ()) = tokio::join!(
        handler.connect("11:22:33:44:55:66", on_disconnect, false),
        async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            handler.stop_scan().await.expect("stop_scan failed");
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(!handler.is_scanning().await);
            assert!(world.is_scanning(), "the connect stopped looking");
        }
    );
    assert!(
        matches!(result, Err(Error::UnknownPeripheral(_))),
        "{:?}",
        result.map(|_| ())
    );
    assert!(!handler.is_scanning().await);
    assert!(!world.is_scanning(), "adapter left scanning");
    assert_eq!(drain(&mut updates), vec![true, false]);
    while rx.recv().await.is_some() {}
}

/// A connect the caller gives up on (a timeout around it) drops its future
/// half way; the scan it paused has to go on all the same.
#[tokio::test(start_paused = true)]
async fn app_scan_survives_a_cancelled_connect() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let late = late_device(&world);
    scan(handler, ScanFilter::None).await.expect("scan failed");
    device.set_connect_behaviour(ConnectBehaviour::Hang);
    let mut updates = scanning_updates(handler).await;

    let (rx, start) = app_scan(handler, ScanFilter::None).await;
    settle().await;

    let cancelled =
        tokio::time::timeout(Duration::from_millis(500), connect_to(handler, &device)).await;
    assert!(cancelled.is_err(), "the connect did not hang");
    assert!(!world.is_scanning(), "scanning while connecting");
    settle().await;

    assert_scan_goes_on(&world, handler, rx, start, &mut updates, &late).await;
}
