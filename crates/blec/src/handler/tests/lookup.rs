//! Finding the characteristic an operation targets. `CHARAC` exists in both
//! services of the stress device, `CHARAC_NOTIFY` only in `SERVICE`.

use super::helpers::*;
use crate::error::Error;
use crate::mock::Op;
use crate::models::WriteType;

fn assert_ambiguous<T: std::fmt::Debug>(result: Result<T, Error>) {
    match result {
        Err(Error::AmbiguousCharacteristic { uuid, mut services }) => {
            assert_eq!(uuid, CHARAC);
            services.sort();
            assert_eq!(services, vec![SERVICE, SERVICE2]);
        }
        other => panic!("expected AmbiguousCharacteristic, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn uuid_in_several_services_needs_the_service() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    connect_ok(handler, &device).await;

    assert_ambiguous(
        handler
            .send_data(CHARAC, None, &[1], WriteType::WithResponse)
            .await,
    );
    assert_ambiguous(handler.recv_data(CHARAC, None).await);
    assert_ambiguous(handler.subscribe(CHARAC, None, |_: Vec<u8>| {}).await);
    // nothing reached the device
    assert_eq!(device.value(SERVICE, CHARAC), None);
    assert_eq!(device.value(SERVICE2, CHARAC), None);
    assert!(device.subscriptions().is_empty());

    handler
        .send_data(CHARAC, Some(SERVICE2), &[2], WriteType::WithResponse)
        .await
        .unwrap();
    assert_eq!(device.value(SERVICE, CHARAC), None);
    assert_eq!(device.value(SERVICE2, CHARAC), Some(vec![2]));
}

#[tokio::test(start_paused = true)]
async fn uuid_in_one_service_needs_no_service() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    connect_ok(handler, &device).await;

    handler
        .subscribe(CHARAC_NOTIFY, None, |_: Vec<u8>| {})
        .await
        .unwrap();
    assert!(device.subscriptions().contains(&(SERVICE, CHARAC_NOTIFY)));
}

#[tokio::test(start_paused = true)]
async fn unsubscribe_without_service_covers_every_service() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    connect_ok(handler, &device).await;
    for service in [SERVICE, SERVICE2] {
        handler
            .subscribe(CHARAC, Some(service), |_: Vec<u8>| {})
            .await
            .unwrap();
    }
    assert_eq!(device.subscriptions().len(), 2);

    handler.unsubscribe(CHARAC).await.unwrap();

    assert!(device.subscriptions().is_empty());
    let unsubscribes = device
        .ops()
        .into_iter()
        .filter(|op| *op == Op::Unsubscribe(CHARAC))
        .count();
    assert_eq!(unsubscribes, 2);
}
