//! The handler driven from an executor that is not tokio.
//!
//! These run on plain threads with `futures_lite::future::block_on`, so time
//! is real: timeouts are kept short and waiting is done with thread sleeps
//! (the handler's own work runs on blec's fallback runtime meanwhile).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_lite::future::{block_on, poll_once};
use tokio::sync::mpsc;

use super::helpers::*;
use crate::error::Error;
use crate::handler::Handler;
use crate::mock::{ConnectBehaviour, DeviceHandle, OpBehaviour};
use crate::models::{ScanFilter, Timeouts, WriteType};

/// Polls `cond` in real time until it holds or `max` passed.
fn wait_until_real(max: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + max;
    while !cond() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

fn assert_no_tokio() {
    assert!(
        tokio::runtime::Handle::try_current().is_err(),
        "test must not run inside a tokio runtime"
    );
}

/// Scans until `device` was seen, so `connect` can find it.
async fn scan_for(handler: &'static Handler, device: &DeviceHandle) {
    let (tx, mut rx) = mpsc::channel(64);
    handler
        .discover(Some(tx), 400, ScanFilter::None, false)
        .await
        .expect("discover failed");
    let address = device.address_string();
    while let Some(devices) = rx.recv().await {
        if devices.iter().any(|d| d.address == address) {
            break;
        }
    }
    handler.stop_scan().await.expect("stop_scan failed");
}

#[test]
fn works_without_tokio_runtime() {
    assert_no_tokio();
    let (world, handler) = test_world();
    let device = stress_device(&world);
    block_on(async {
        scan_for(handler, &device).await;
        let (on_disconnect, _) = disconnect_counter();
        let connection = handler
            .connect(&device.address_string(), on_disconnect, false)
            .await
            .expect("connect failed");
        assert!(device.is_connected());

        connection
            .send_data(CHARAC, Some(SERVICE), &[1, 2, 3], WriteType::WithResponse)
            .await
            .expect("write failed");
        assert_eq!(device.value(SERVICE, CHARAC), Some(vec![1, 2, 3]));
        device.set_value(SERVICE, CHARAC, vec![4, 5]);
        assert_eq!(
            connection.recv_data(CHARAC, Some(SERVICE)).await.unwrap(),
            vec![4, 5]
        );

        let received = Arc::new(Mutex::new(vec![]));
        let r = received.clone();
        connection
            .subscribe(CHARAC_NOTIFY, None, move |data| {
                r.lock().unwrap().push(data)
            })
            .await
            .expect("subscribe failed");
        // the notification task is spawned on the fallback runtime
        assert!(wait_until_real(Duration::from_secs(2), || {
            device.notify(SERVICE, CHARAC_NOTIFY, vec![9]);
            !received.lock().unwrap().is_empty()
        }));

        connection.disconnect().await.expect("disconnect failed");
        assert!(!device.is_connected());
    });
}

#[test]
fn timeouts_fire_without_tokio_runtime() {
    assert_no_tokio();
    let (world, handler) = test_world();
    handler.set_timeouts(Timeouts {
        read: Duration::from_millis(100),
        ..short_timeouts()
    });
    let device = stress_device(&world);
    block_on(async {
        scan_for(handler, &device).await;
        let (on_disconnect, _) = disconnect_counter();
        let connection = handler
            .connect(&device.address_string(), on_disconnect, false)
            .await
            .expect("connect failed");
        device.set_read_behaviour(OpBehaviour::Hang);
        let start = Instant::now();
        let result = connection.recv_data(CHARAC, Some(SERVICE)).await;
        assert!(matches!(result, Err(Error::Timeout(_))), "got {result:?}");
        assert!(start.elapsed() < Duration::from_secs(2));
    });
}

#[test]
fn cancelled_connect_without_tokio_runtime() {
    assert_no_tokio();
    let (world, handler) = test_world();
    let device = stress_device(&world);
    block_on(async {
        scan_for(handler, &device).await;
        device.set_connect_behaviour(ConnectBehaviour::Hang);
        let (on_disconnect, _) = disconnect_counter();
        let address = device.address_string();
        let mut connecting = Box::pin(handler.connect(&address, on_disconnect, false));
        assert!(poll_once(&mut connecting).await.is_none());
        // dropping it outside of any runtime must not panic
        drop(connecting);

        device.set_connect_behaviour(ConnectBehaviour::Ok);
        let (on_disconnect, _) = disconnect_counter();
        handler
            .connect(&device.address_string(), on_disconnect, false)
            .await
            .expect("connect after a cancelled one failed");
    });
}
