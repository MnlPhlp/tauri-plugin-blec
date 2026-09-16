use std::sync::atomic::Ordering;
use std::time::Duration;

use btleplug::api::CentralState;

use super::helpers::*;
use crate::error::Error;
use crate::mock::{FailKind, Op, OpBehaviour};
use crate::models::WriteType;

#[tokio::test(start_paused = true)]
async fn user_disconnect() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let mut updates = connection_updates(handler).await;
    let counter = connect_ok(handler, &device).await;
    drain(&mut updates);

    handler.disconnect().await.unwrap();
    assert!(!device.is_connected());
    assert_disconnected(handler).await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert_eq!(drain(&mut updates), vec![false]);

    // disconnecting again is an error, nothing else happens
    assert!(matches!(
        handler.disconnect().await,
        Err(Error::NoDeviceConnected)
    ));
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    connect_ok(handler, &device).await;
}

#[tokio::test(start_paused = true)]
async fn link_drops_mid_session() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let mut updates = connection_updates(handler).await;
    let counter = connect_ok(handler, &device).await;
    drain(&mut updates);

    assert!(device.drop_link());
    assert!(
        wait_until(Duration::from_secs(1), || !handler.is_connected()).await,
        "handler did not notice the dropped link"
    );
    settle().await;
    assert_disconnected(handler).await;
    assert_eq!(counter.load(Ordering::SeqCst), 1, "callback count");
    assert_eq!(drain(&mut updates), vec![false]);
    assert!(matches!(
        handler
            .send_data(CHARAC, None, &[1], WriteType::WithResponse)
            .await,
        Err(Error::NoDeviceConnected)
    ));

    let counter = connect_ok(handler, &device).await;
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn disconnect_when_link_silently_gone() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let counter = connect_ok(handler, &device).await;

    // the adapter lost the link but never said so
    assert!(device.drop_link_silently());
    settle().await;
    assert!(
        handler.is_connected(),
        "precondition: handler still believes it is connected"
    );

    handler.disconnect().await.unwrap();
    assert_disconnected(handler).await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    connect_ok(handler, &device).await;
}

#[tokio::test(start_paused = true)]
async fn operation_on_silently_gone_link_fails_without_state_change() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let counter = connect_ok(handler, &device).await;
    assert!(device.drop_link_silently());

    let result = handler
        .send_data(CHARAC, None, &[1], WriteType::WithResponse)
        .await;
    assert!(matches!(result, Err(Error::Btleplug(_))), "{result:?}");
    // nobody told the handler; it must still allow a clean disconnect
    handler.disconnect().await.unwrap();
    assert_disconnected(handler).await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn disconnect_without_disconnect_event() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let counter = connect_ok(handler, &device).await;
    device.set_disconnect_emits_event(false);

    let start = tokio::time::Instant::now();
    handler.disconnect().await.unwrap();
    assert!(
        start.elapsed() >= short_timeouts().disconnect,
        "did not wait for the disconnect event"
    );
    assert!(!device.is_connected());
    assert_disconnected(handler).await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn disconnect_call_fails() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let counter = connect_ok(handler, &device).await;
    device.set_disconnect_behaviour(OpBehaviour::Fail(FailKind::Runtime("busy".into())));

    // the error is logged, the state is repaired after the timeout
    handler.disconnect().await.unwrap();
    assert_disconnected(handler).await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn adapter_powered_off_while_connected() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let mut updates = connection_updates(handler).await;
    let counter = connect_ok(handler, &device).await;
    drain(&mut updates);

    world.set_adapter_state(CentralState::PoweredOff);
    assert!(
        wait_until(Duration::from_secs(1), || !handler.is_connected()).await,
        "handler did not react to the adapter powering off"
    );
    settle().await;
    assert_disconnected(handler).await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert_eq!(drain(&mut updates), vec![false]);
    assert!(matches!(
        handler.get_adapter_state().await,
        crate::models::AdapterState::Off
    ));

    world.set_adapter_state(CentralState::PoweredOn);
    settle().await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    connect_ok(handler, &device).await;
}

#[tokio::test(start_paused = true)]
async fn foreign_disconnect_event_is_ignored() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let other = other_device(&world);
    let counter = connect_ok(handler, &device).await;

    other.emit_disconnected_event();
    settle().await;
    assert!(handler.is_connected());
    assert!(device.is_connected());
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn spurious_connect_event_is_hung_up() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let other = other_device(&world);
    scan(handler, crate::models::ScanFilter::None)
        .await
        .unwrap();

    // a link nobody asked for
    other.complete_connect();
    assert!(
        wait_until(Duration::from_secs(1), || !other.is_connected()).await,
        "unrequested link was not closed"
    );
    assert_eq!(other.ops(), vec![Op::Disconnect]);
    assert_disconnected(handler).await;

    // while connected to `device` a foreign connect event is hung up too
    let counter = connect_ok(handler, &device).await;
    other.clear_ops();
    other.complete_connect();
    assert!(wait_until(Duration::from_secs(1), || !other.is_connected()).await);
    assert!(handler.is_connected());
    assert!(device.is_connected());
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn duplicate_disconnect_events_run_callback_once() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let counter = connect_ok(handler, &device).await;

    device.drop_link();
    device.emit_disconnected_event();
    device.emit_disconnected_event();
    settle().await;
    assert_disconnected(handler).await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn link_drops_right_after_connect() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    device.set_connect_behaviour(crate::mock::ConnectBehaviour::OkThenDrop(
        Duration::from_millis(1),
    ));

    // The drop may hit before or after service discovery; either way the
    // handler must end up disconnected with the callback run at most once.
    let (result, counter) = connect(handler, &device).await;
    settle().await;
    assert!(
        wait_until(Duration::from_secs(3), || !handler.is_connected()).await,
        "handler stayed connected after the link dropped, connect result {result:?}"
    );
    settle().await;
    assert_disconnected(handler).await;
    let callbacks = counter.load(Ordering::SeqCst);
    assert!(
        (result.is_ok() && callbacks == 1) || (result.is_err() && callbacks == 0),
        "connect result {result:?} but callback ran {callbacks} times"
    );

    device.set_connect_behaviour(crate::mock::ConnectBehaviour::Ok);
    connect_ok(handler, &device).await;
}

#[tokio::test(start_paused = true)]
async fn link_drops_during_service_discovery() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    device.set_discover_behaviour(OpBehaviour::Delay(Duration::from_millis(500)));

    let dropper = device.clone();
    tokio::spawn(async move {
        // connect (scan 1 s) is done and discovery is running
        let _ = wait_until(Duration::from_secs(5), || dropper.is_connected()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        dropper.drop_link();
    });

    let (result, counter) = connect(handler, &device).await;
    assert!(result.is_err(), "connect succeeded on a dropped link");
    settle().await;
    assert_disconnected(handler).await;
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "callback of a failed connect ran"
    );
}
