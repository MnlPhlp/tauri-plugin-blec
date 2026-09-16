//! Shared fixtures for the handler tests.
//!
//! Every test gets its own [`MockWorld`] and its own leaked `&'static Handler`
//! (the handler API needs `'static` because it spawns tasks that borrow it).
//! Tests run with `start_paused = true`: the handler's retry sleeps and all
//! timeouts elapse instantly, without becoming flaky.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use btleplug::api::CharPropFlags;
use once_cell::sync::Lazy;
use tokio::sync::mpsc;
use uuid::{uuid, Uuid};

use crate::error::Error;
use crate::handler::{Handler, OnDisconnectHandler};
use crate::mock::{DeviceHandle, DeviceSpec, MockWorld, ServiceSpec};
use crate::models::{BleDevice, ScanFilter, Timeouts};

pub const SERVICE: Uuid = uuid!("A07498CA-AD5B-474E-940D-16F1FBE7E8CD");
pub const SERVICE2: Uuid = uuid!("A07498CA-AD5B-474E-940D-16F1FBE7E8CE");
/// Present in both services, like on the test server.
pub const CHARAC: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");
/// Notify only characteristic in `SERVICE`.
pub const CHARAC_NOTIFY: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021C");
/// Characteristic the device does not have.
pub const CHARAC_MISSING: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC0000");
pub const MANUFACTURER_ID: u16 = 0xf00d;
pub const MANUFACTURER_DATA: [u8; 4] = [0x21, 0x22, 0x23, 0x24];

pub const DEVICE_ADDR: [u8; 6] = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01];
pub const OTHER_ADDR: [u8; 6] = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x02];

/// Timeouts short enough that even the retrying `connect` finishes quickly.
pub fn short_timeouts() -> Timeouts {
    Timeouts {
        connect: Duration::from_secs(2),
        discover_services: Duration::from_secs(1),
        read: Duration::from_secs(1),
        write: Duration::from_secs(1),
        subscribe: Duration::from_secs(1),
        disconnect: Duration::from_secs(1),
    }
}

static TRACING: Lazy<()> = Lazy::new(|| {
    // `RUST_LOG=trace cargo test -- --nocapture` shows the handler's logs
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
});

/// A fresh world and a handler bound to its adapter.
pub fn test_world() -> (MockWorld, &'static Handler) {
    Lazy::force(&TRACING);
    let world = MockWorld::new();
    let handler: &'static Handler = Box::leak(Box::new(Handler::new_with_adapter(world.adapter())));
    handler.set_timeouts(short_timeouts());
    (world, handler)
}

/// The device most tests talk to: two services sharing a characteristic uuid
/// plus a notify only characteristic, mirroring `examples/test-server`.
pub fn stress_device(world: &MockWorld) -> DeviceHandle {
    world.add_device(
        DeviceSpec::new(DEVICE_ADDR)
            .name("stress")
            .manufacturer_data(MANUFACTURER_ID, MANUFACTURER_DATA.to_vec())
            .service(
                ServiceSpec::new(SERVICE)
                    .characteristic(
                        CHARAC,
                        CharPropFlags::READ
                            | CharPropFlags::WRITE
                            | CharPropFlags::WRITE_WITHOUT_RESPONSE
                            | CharPropFlags::NOTIFY,
                    )
                    .characteristic(CHARAC_NOTIFY, CharPropFlags::NOTIFY),
            )
            .service(ServiceSpec::new(SERVICE2).characteristic(
                CHARAC,
                CharPropFlags::READ | CharPropFlags::WRITE | CharPropFlags::NOTIFY,
            )),
    )
}

/// A second, unrelated device without services.
pub fn other_device(world: &MockWorld) -> DeviceHandle {
    world.add_device(DeviceSpec::new(OTHER_ADDR).name("other"))
}

/// A disconnect callback that counts how often it ran.
pub fn disconnect_counter() -> (OnDisconnectHandler, Arc<AtomicUsize>) {
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();
    (
        OnDisconnectHandler::from_sync(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }),
        counter,
    )
}

/// Registers a connection update channel and returns its receiver.
pub async fn connection_updates(handler: &Handler) -> mpsc::Receiver<bool> {
    let (tx, rx) = mpsc::channel(64);
    handler.set_connection_update_channel(tx).await;
    rx
}

/// Registers a scanning update channel and returns its receiver.
pub async fn scanning_updates(handler: &Handler) -> mpsc::Receiver<bool> {
    let (tx, rx) = mpsc::channel(64);
    handler.set_scanning_update_channel(tx).await;
    rx
}

/// Everything currently queued in an update channel.
pub fn drain(rx: &mut mpsc::Receiver<bool>) -> Vec<bool> {
    let mut out = vec![];
    while let Ok(v) = rx.try_recv() {
        out.push(v);
    }
    out
}

/// Polls `cond` until it holds or `max` (virtual) time passed. Returns whether it holds.
pub async fn wait_until(max: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + max;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return cond();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Lets every other task run until it blocks (a few ms of virtual time).
pub async fn settle() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// Runs a scan with `filter` and returns the final device list.
pub async fn scan(handler: &'static Handler, filter: ScanFilter) -> Result<Vec<BleDevice>, Error> {
    let (tx, mut rx) = mpsc::channel(64);
    handler.discover(Some(tx), 400, filter, false).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while handler.is_scanning().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "scan did not finish"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    settle().await;
    let mut last = vec![];
    while let Ok(devices) = rx.try_recv() {
        last = devices;
    }
    Ok(last)
}

/// Scans, then connects to `device` with a counting disconnect callback.
///
/// The scan is explicit because `connect`'s own scan-if-unknown path is
/// broken, see `connect::connect_without_prior_scan`.
pub async fn connect(
    handler: &'static Handler,
    device: &DeviceHandle,
) -> (Result<(), Error>, Arc<AtomicUsize>) {
    scan(handler, ScanFilter::None).await.expect("scan failed");
    let (on_disconnect, counter) = disconnect_counter();
    let result = handler
        .connect(&device.address_string(), on_disconnect, false)
        .await;
    // let the notification task the handler spawned subscribe to the device;
    // on real hardware the subscription round trip guarantees that
    settle().await;
    (result, counter)
}

/// Connects and asserts success.
pub async fn connect_ok(handler: &'static Handler, device: &DeviceHandle) -> Arc<AtomicUsize> {
    let (result, counter) = connect(handler, device).await;
    result.expect("connect failed");
    assert!(
        handler.is_connected(),
        "handler not connected after connect"
    );
    assert!(device.is_connected(), "device not connected after connect");
    counter
}

/// Asserts that the handler holds no connection state at all.
pub async fn assert_disconnected(handler: &Handler) {
    assert!(!handler.is_connected(), "is_connected() still true");
    assert!(
        matches!(
            handler.connected_device().await,
            Err(Error::NoDeviceConnected)
        ),
        "connected_device() still returns a device"
    );
    assert!(
        matches!(handler.mtu().await, Err(Error::NoDeviceConnected)),
        "mtu() still works"
    );
    assert!(
        matches!(
            handler.recv_data(CHARAC, None).await,
            Err(Error::NoDeviceConnected)
        ),
        "recv_data() does not report NoDeviceConnected"
    );
}
