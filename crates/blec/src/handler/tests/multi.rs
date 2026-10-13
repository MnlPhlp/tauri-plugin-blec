//! Several devices connected at the same time.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use btleplug::api::CentralState;

use super::helpers::*;
use crate::error::Error;
use crate::mock::{FailKind, OpBehaviour};
use crate::models::WriteType;

type Received = Arc<Mutex<Vec<Vec<u8>>>>;

fn recorder() -> (Received, impl Fn(Vec<u8>) + Send + Sync + 'static) {
    let received: Received = Arc::new(Mutex::new(vec![]));
    let r = received.clone();
    (received, move |data| r.lock().unwrap().push(data))
}

#[tokio::test(start_paused = true)]
async fn operations_are_routed_by_address() {
    let (world, handler) = test_world();
    let first = stress_device(&world);
    let second = second_device(&world);
    let (a, _) = connect_conn_ok(handler, &first).await;
    let (b, _) = connect_conn_ok(handler, &second).await;

    a.send_data(CHARAC, Some(SERVICE), &[1], WriteType::WithResponse)
        .await
        .unwrap();
    handler
        .send_data_to(
            &second.address_string(),
            CHARAC,
            None,
            &[2],
            WriteType::WithResponse,
        )
        .await
        .unwrap();
    assert_eq!(first.value(SERVICE, CHARAC), Some(vec![1]));
    assert_eq!(second.value(SERVICE, CHARAC), Some(vec![2]));
    assert_eq!(b.recv_data(CHARAC, None).await.unwrap(), vec![2]);
    assert_eq!(
        handler
            .recv_data_from(&first.address_string(), CHARAC, Some(SERVICE))
            .await
            .unwrap(),
        vec![1]
    );

    // the characteristics are the ones of each device
    a.send_data(CHARAC, Some(SERVICE2), &[3], WriteType::WithResponse)
        .await
        .unwrap();
    assert!(matches!(
        b.send_data(CHARAC, Some(SERVICE2), &[3], WriteType::WithResponse)
            .await,
        Err(Error::CharacNotAvailable(_))
    ));

    assert_eq!(a.mtu().await.unwrap(), 247);
    assert_eq!(b.mtu().await.unwrap(), SECOND_MTU);
    assert_eq!(b.device().await.unwrap().address, second.address_string());
    assert_eq!(
        handler.connection(&first.address_string()).unwrap(),
        a,
        "connection() hands out the same handle"
    );
    assert!(matches!(
        handler.connection("11:22:33:44:55:66"),
        Err(Error::DeviceNotConnected(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn notifications_are_per_device() {
    let (world, handler) = test_world();
    let first = stress_device(&world);
    let second = second_device(&world);
    let (a, _) = connect_conn_ok(handler, &first).await;
    let (b, _) = connect_conn_ok(handler, &second).await;
    let (from_a, callback_a) = recorder();
    let (from_b, callback_b) = recorder();
    a.subscribe(CHARAC_NOTIFY, None, callback_a).await.unwrap();
    b.subscribe(CHARAC_NOTIFY, None, callback_b).await.unwrap();

    assert!(first.notify(SERVICE, CHARAC_NOTIFY, vec![1]));
    assert!(second.notify(SERVICE, CHARAC_NOTIFY, vec![2]));
    assert!(first.notify(SERVICE, CHARAC_NOTIFY, vec![3]));
    settle().await;
    assert_eq!(*from_a.lock().unwrap(), vec![vec![1], vec![3]]);
    assert_eq!(*from_b.lock().unwrap(), vec![vec![2]]);

    // unsubscribing on one device leaves the other subscribed
    a.unsubscribe(CHARAC_NOTIFY, None).await.unwrap();
    assert!(first.subscriptions().is_empty());
    assert!(second.notify(SERVICE, CHARAC_NOTIFY, vec![4]));
    settle().await;
    assert_eq!(*from_b.lock().unwrap(), vec![vec![2], vec![4]]);
}

#[tokio::test(start_paused = true)]
async fn unsubscribe_from_is_scoped_to_service() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let (a, _) = connect_conn_ok(handler, &device).await;
    let (service1, callback1) = recorder();
    let (service2, callback2) = recorder();
    a.subscribe(CHARAC, Some(SERVICE), callback1).await.unwrap();
    a.subscribe(CHARAC, Some(SERVICE2), callback2)
        .await
        .unwrap();

    a.unsubscribe(CHARAC, Some(SERVICE2)).await.unwrap();
    assert!(device.notify(SERVICE, CHARAC, vec![1]));
    assert!(!device.notify(SERVICE2, CHARAC, vec![2]));
    settle().await;
    assert_eq!(*service1.lock().unwrap(), vec![vec![1]]);
    assert!(service2.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn dropping_one_link_keeps_the_other() {
    let (world, handler) = test_world();
    let first = stress_device(&world);
    let second = second_device(&world);
    let (a, counter_a) = connect_conn_ok(handler, &first).await;
    let (b, counter_b) = connect_conn_ok(handler, &second).await;
    let (_, callback_a) = recorder();
    let (from_b, callback_b) = recorder();
    a.subscribe(CHARAC_NOTIFY, None, callback_a).await.unwrap();
    b.subscribe(CHARAC_NOTIFY, None, callback_b).await.unwrap();
    let mut updates = connection_updates(handler).await;
    let mut events = connection_events(handler).await;

    assert!(first.drop_link());
    assert!(
        wait_until(Duration::from_secs(1), || !a.is_connected()).await,
        "handler did not notice the dropped link"
    );
    settle().await;

    assert_device_disconnected(handler, &first).await;
    assert_eq!(counter_a.load(Ordering::SeqCst), 1);
    assert_eq!(counter_b.load(Ordering::SeqCst), 0);
    assert_eq!(drain(&mut events), vec![update(&first, false)]);
    // somebody is still connected
    assert_eq!(drain(&mut updates), vec![true]);
    assert!(handler.is_connected());

    assert!(b.is_connected());
    assert!(second.is_connected());
    assert!(second.notify(SERVICE, CHARAC_NOTIFY, vec![7]));
    settle().await;
    assert_eq!(*from_b.lock().unwrap(), vec![vec![7]]);
    b.send_data(CHARAC, None, &[8], WriteType::WithResponse)
        .await
        .unwrap();
    assert!(matches!(
        a.recv_data(CHARAC, None).await,
        Err(Error::DeviceNotConnected(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn address_less_api_needs_a_single_device() {
    let (world, handler) = test_world();
    let first = stress_device(&world);
    let second = second_device(&world);
    connect_ok(handler, &first).await;
    let (b, _) = connect_conn_ok(handler, &second).await;

    assert!(matches!(
        handler
            .send_data(CHARAC, None, &[1], WriteType::WithResponse)
            .await,
        Err(Error::MultipleDevicesConnected)
    ));
    assert!(matches!(
        handler.recv_data(CHARAC, None).await,
        Err(Error::MultipleDevicesConnected)
    ));
    assert!(matches!(
        handler.subscribe(CHARAC_NOTIFY, None, |_| {}).await,
        Err(Error::MultipleDevicesConnected)
    ));
    assert!(matches!(
        handler.unsubscribe(CHARAC_NOTIFY).await,
        Err(Error::MultipleDevicesConnected)
    ));
    assert!(matches!(
        handler.mtu().await,
        Err(Error::MultipleDevicesConnected)
    ));
    assert!(matches!(
        handler.connected_device().await,
        Err(Error::MultipleDevicesConnected)
    ));
    assert!(matches!(
        handler.disconnect().await,
        Err(Error::MultipleDevicesConnected)
    ));
    assert!(first.is_connected() && second.is_connected());

    // with one device left, they work on that one
    b.disconnect().await.unwrap();
    assert_device_disconnected(handler, &second).await;
    assert_eq!(
        handler.connected_device().await.unwrap().address,
        first.address_string()
    );
    handler
        .send_data(CHARAC, None, &[5], WriteType::WithResponse)
        .await
        .unwrap();
    assert_eq!(first.value(SERVICE, CHARAC), Some(vec![5]));
    handler.disconnect().await.unwrap();
    assert_disconnected(handler).await;
}

#[tokio::test(start_paused = true)]
async fn disconnect_all_disconnects_every_device() {
    let (world, handler) = test_world();
    let first = stress_device(&world);
    let second = second_device(&world);
    let counter_a = connect_ok(handler, &first).await;
    let counter_b = connect_ok(handler, &second).await;
    let mut events = connection_events(handler).await;

    handler.disconnect_all().await.unwrap();
    assert!(!first.is_connected());
    assert!(!second.is_connected());
    assert_disconnected(handler).await;
    assert_eq!(counter_a.load(Ordering::SeqCst), 1);
    assert_eq!(counter_b.load(Ordering::SeqCst), 1);
    let mut events = drain(&mut events);
    events.sort_by(|a, b| a.address.cmp(&b.address));
    assert_eq!(events, vec![update(&first, false), update(&second, false)]);

    // nothing connected: nothing to do
    handler.disconnect_all().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn adapter_powered_off_clears_every_link() {
    let (world, handler) = test_world();
    let first = stress_device(&world);
    let second = second_device(&world);
    let counter_a = connect_ok(handler, &first).await;
    let counter_b = connect_ok(handler, &second).await;

    world.set_adapter_state(CentralState::PoweredOff);
    assert!(
        wait_until(Duration::from_secs(1), || !handler.is_connected()).await,
        "handler did not react to the adapter powering off"
    );
    settle().await;
    assert_disconnected(handler).await;
    assert_eq!(counter_a.load(Ordering::SeqCst), 1);
    assert_eq!(counter_b.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn failed_connect_leaves_other_device_connected() {
    let (world, handler) = test_world();
    let first = stress_device(&world);
    let second = second_device(&world);
    let (a, counter_a) = connect_conn_ok(handler, &first).await;
    let mut events = connection_events(handler).await;
    second.fail_next_connects(3, FailKind::Runtime("busy".into()));

    let (result, counter_b) = connect(handler, &second).await;
    assert!(result.is_err(), "{result:?}");
    assert_device_disconnected(handler, &second).await;
    assert_eq!(counter_b.load(Ordering::SeqCst), 0);

    assert!(a.is_connected());
    assert!(first.is_connected());
    assert_eq!(counter_a.load(Ordering::SeqCst), 0);
    // like with a single device, listeners hear that the attempt ended
    assert_eq!(drain(&mut events), vec![update(&second, false)]);
    a.send_data(CHARAC, None, &[1], WriteType::WithResponse)
        .await
        .unwrap();
    // the failed attempt left nothing behind for the address-less API
    assert_eq!(
        handler.connected_device().await.unwrap().address,
        first.address_string()
    );
}

#[tokio::test(start_paused = true)]
async fn connection_handle_survives_reconnect() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let (a, _) = connect_conn_ok(handler, &device).await;

    device.drop_link();
    assert!(wait_until(Duration::from_secs(1), || !a.is_connected()).await);
    settle().await;
    assert!(matches!(
        a.recv_data(CHARAC, None).await,
        Err(Error::DeviceNotConnected(_))
    ));
    assert!(matches!(
        a.disconnect().await,
        Err(Error::DeviceNotConnected(_))
    ));

    connect_ok(handler, &device).await;
    assert!(a.is_connected());
    a.send_data(CHARAC, None, &[9], WriteType::WithResponse)
        .await
        .unwrap();
    assert_eq!(device.value(SERVICE, CHARAC), Some(vec![9]));
}

#[tokio::test(start_paused = true)]
async fn discover_services_of_another_device_while_connected() {
    let (world, handler) = test_world();
    let first = stress_device(&world);
    let second = second_device(&world);
    let counter_a = connect_ok(handler, &first).await;
    let mut updates = connection_updates(handler).await;
    let mut events = connection_events(handler).await;

    let services = handler
        .discover_services(&second.address_string())
        .await
        .unwrap();
    assert_eq!(
        services.iter().map(|s| s.uuid).collect::<Vec<_>>(),
        vec![SERVICE]
    );
    settle().await;
    assert!(!second.is_connected(), "discovery link left open");
    assert_device_disconnected(handler, &second).await;
    assert!(
        drain(&mut updates).is_empty(),
        "discovery link was reported"
    );
    assert!(drain(&mut events).is_empty(), "discovery link was reported");

    assert!(first.is_connected());
    assert_eq!(counter_a.load(Ordering::SeqCst), 0);
    assert_eq!(
        handler.connected_device().await.unwrap().address,
        first.address_string()
    );

    // the services of a connected device come from its link
    let services = handler
        .discover_services(&first.address_string())
        .await
        .unwrap();
    assert_eq!(services.len(), 2);
    assert!(first.is_connected());
}

/// GATT operations are serialized per device, but not across devices.
#[tokio::test(start_paused = true)]
async fn operations_run_in_parallel_across_devices() {
    let (world, handler) = test_world();
    let first = stress_device(&world);
    let second = second_device(&world);
    let (a, _) = connect_conn_ok(handler, &first).await;
    let (b, _) = connect_conn_ok(handler, &second).await;
    let delay = Duration::from_millis(400);
    first.set_read_behaviour(OpBehaviour::Delay(delay));
    second.set_read_behaviour(OpBehaviour::Delay(delay));

    let start = tokio::time::Instant::now();
    let (ra, rb) = tokio::join!(a.recv_data(CHARAC, None), b.recv_data(CHARAC, None));
    ra.unwrap();
    rb.unwrap();
    assert!(
        start.elapsed() < delay * 2,
        "reads on two devices were serialized: {:?}",
        start.elapsed()
    );

    let start = tokio::time::Instant::now();
    let (r1, r2) = tokio::join!(a.recv_data(CHARAC, None), a.recv_data(CHARAC, None));
    r1.unwrap();
    r2.unwrap();
    assert!(
        start.elapsed() >= delay * 2,
        "reads on one device overlapped: {:?}",
        start.elapsed()
    );
}
