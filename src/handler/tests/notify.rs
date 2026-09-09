use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::helpers::*;
use crate::error::Error;
use crate::handler::SubscriptionHandler;
use crate::mock::{FailKind, Op, OpBehaviour};

type Received = Arc<Mutex<Vec<Vec<u8>>>>;

fn recorder() -> (Received, impl Fn(Vec<u8>) + Send + Sync + 'static) {
    let received: Received = Arc::new(Mutex::new(vec![]));
    let r = received.clone();
    (received, move |data| r.lock().unwrap().push(data))
}

#[tokio::test(start_paused = true)]
async fn notifications_are_delivered_in_order() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    connect_ok(handler, &device).await;
    let (received, callback) = recorder();

    handler
        .subscribe(CHARAC_NOTIFY, None, callback)
        .await
        .unwrap();
    assert!(device.subscriptions().contains(&(SERVICE, CHARAC_NOTIFY)));

    for i in 0..100u32 {
        assert!(device.notify(SERVICE, CHARAC_NOTIFY, i.to_le_bytes().to_vec()));
    }
    assert!(
        wait_until(Duration::from_secs(1), || received.lock().unwrap().len()
            == 100)
        .await,
        "got {} of 100 notifications",
        received.lock().unwrap().len()
    );
    let expected: Vec<Vec<u8>> = (0..100u32).map(|i| i.to_le_bytes().to_vec()).collect();
    assert_eq!(*received.lock().unwrap(), expected);
}

#[tokio::test(start_paused = true)]
async fn subscription_is_scoped_to_service() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    connect_ok(handler, &device).await;
    let (received, callback) = recorder();

    // CHARAC exists in SERVICE and SERVICE2; subscribe to the SERVICE2 one
    handler
        .subscribe(CHARAC, Some(SERVICE2), callback)
        .await
        .unwrap();
    assert_eq!(
        device.subscriptions(),
        [(SERVICE2, CHARAC)].into_iter().collect()
    );

    // a notification of the same uuid in the other service is not delivered
    // (the device would not send it, but even a spurious one must be filtered)
    assert!(!device.notify(SERVICE, CHARAC, vec![1]));
    assert!(device.notify(SERVICE2, CHARAC, vec![2]));
    settle().await;
    assert_eq!(*received.lock().unwrap(), vec![vec![2]]);
}

#[tokio::test(start_paused = true)]
async fn unsubscribe_stops_delivery() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    connect_ok(handler, &device).await;
    let (received, callback) = recorder();

    handler
        .subscribe(CHARAC_NOTIFY, None, callback)
        .await
        .unwrap();
    device.notify(SERVICE, CHARAC_NOTIFY, vec![1]);
    settle().await;
    handler.unsubscribe(CHARAC_NOTIFY).await.unwrap();
    assert!(device.subscriptions().is_empty());
    assert!(!device.notify(SERVICE, CHARAC_NOTIFY, vec![2]));
    settle().await;
    assert_eq!(*received.lock().unwrap(), vec![vec![1]]);
    assert!(device.ops().contains(&Op::Unsubscribe(CHARAC_NOTIFY)));
}

#[tokio::test(start_paused = true)]
async fn subscribe_errors() {
    let (world, handler) = test_world();
    let device = stress_device(&world);

    assert!(matches!(
        handler.subscribe(CHARAC_NOTIFY, None, |_| {}).await,
        Err(Error::NoDeviceConnected)
    ));
    connect_ok(handler, &device).await;
    assert!(matches!(
        handler.subscribe(CHARAC_MISSING, None, |_| {}).await,
        Err(Error::CharacNotAvailable(_))
    ));
    device.set_subscribe_behaviour(OpBehaviour::Fail(FailKind::NotSupported));
    assert!(matches!(
        handler.subscribe(CHARAC_NOTIFY, None, |_| {}).await,
        Err(Error::Btleplug(_))
    ));
    device.set_subscribe_behaviour(OpBehaviour::Hang);
    assert!(matches!(
        handler.subscribe(CHARAC_NOTIFY, None, |_| {}).await,
        Err(Error::Timeout(_))
    ));
    assert!(device.subscriptions().is_empty());
    assert!(handler.is_connected());
}

#[tokio::test(start_paused = true)]
async fn listeners_are_cleared_on_disconnect() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    connect_ok(handler, &device).await;
    let (received, callback) = recorder();
    handler
        .subscribe(CHARAC_NOTIFY, None, callback)
        .await
        .unwrap();

    device.drop_link();
    assert!(wait_until(Duration::from_secs(1), || !handler.is_connected()).await);
    settle().await;

    // reconnect and subscribe with a new callback: the old listener must not
    // receive anything anymore
    connect_ok(handler, &device).await;
    let (_, dummy) = recorder();
    handler.subscribe(CHARAC_NOTIFY, None, dummy).await.unwrap();
    device.notify(SERVICE, CHARAC_NOTIFY, vec![9]);
    settle().await;
    assert!(
        received.lock().unwrap().is_empty(),
        "stale listener received data"
    );
}

#[tokio::test(start_paused = true)]
async fn async_callback_and_burst() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    connect_ok(handler, &device).await;
    let received: Received = Arc::new(Mutex::new(vec![]));
    let r = received.clone();
    handler
        .subscribe(
            CHARAC_NOTIFY,
            None,
            SubscriptionHandler::from_async(move |data| {
                let r = r.clone();
                async move {
                    // a slow consumer
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    r.lock().unwrap().push(data);
                }
            }),
        )
        .await
        .unwrap();

    // 1000 notifications faster than the callback consumes them
    for i in 0..1000u32 {
        device.notify(SERVICE, CHARAC_NOTIFY, i.to_le_bytes().to_vec());
    }
    assert!(
        wait_until(Duration::from_secs(60), || received.lock().unwrap().len()
            == 1000)
        .await,
        "got {} of 1000 notifications",
        received.lock().unwrap().len()
    );
    let first: Vec<u32> = received
        .lock()
        .unwrap()
        .iter()
        .take(5)
        .map(|d| u32::from_le_bytes(d[..].try_into().unwrap()))
        .collect();
    assert_eq!(first, vec![0, 1, 2, 3, 4]);
    assert!(handler.is_connected());
}

#[tokio::test(start_paused = true)]
async fn notification_after_drop_is_not_delivered() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let counter = connect_ok(handler, &device).await;
    let (received, callback) = recorder();
    handler
        .subscribe(CHARAC_NOTIFY, None, callback)
        .await
        .unwrap();

    device.drop_link();
    assert!(!device.notify(SERVICE, CHARAC_NOTIFY, vec![1]));
    settle().await;
    assert!(received.lock().unwrap().is_empty());
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}
