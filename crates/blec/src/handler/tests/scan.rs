use std::time::Duration;

use super::helpers::*;
use crate::models::ScanFilter;

#[tokio::test(start_paused = true)]
async fn scan_finds_devices_in_range() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let other = other_device(&world);

    let devices = scan(handler, ScanFilter::None).await.unwrap();
    let addresses: Vec<_> = devices.iter().map(|d| d.address.clone()).collect();
    assert_eq!(devices.len(), 2, "{addresses:?}");
    assert!(addresses.contains(&device.address_string()));
    assert!(addresses.contains(&other.address_string()));

    let stress = devices
        .iter()
        .find(|d| d.address == device.address_string())
        .unwrap();
    assert_eq!(stress.name, "stress");
    assert_eq!(stress.services, vec![SERVICE, SERVICE2]);
    assert_eq!(
        stress.manufacturer_data.get(&MANUFACTURER_ID),
        Some(&MANUFACTURER_DATA.to_vec())
    );
    assert!(!stress.is_connected);
    assert!(!world.is_scanning(), "scan was not stopped on the adapter");
}

#[tokio::test(start_paused = true)]
async fn out_of_range_device_is_not_found() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let other = other_device(&world);
    device.set_in_range(false);

    let devices = scan(handler, ScanFilter::None).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].address, other.address_string());

    device.set_in_range(true);
    let devices = scan(handler, ScanFilter::None).await.unwrap();
    assert_eq!(devices.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn scan_filters() {
    let (world, handler) = test_world();
    let device = stress_device(&world);
    let _other = other_device(&world);

    let only_stress = |devices: Vec<crate::models::BleDevice>| {
        assert_eq!(devices.len(), 1, "{devices:?}");
        assert_eq!(devices[0].address, device.address_string());
    };

    only_stress(scan(handler, ScanFilter::Service(SERVICE)).await.unwrap());
    only_stress(
        scan(
            handler,
            ScanFilter::AnyService(vec![SERVICE2, CHARAC_MISSING]),
        )
        .await
        .unwrap(),
    );
    only_stress(
        scan(handler, ScanFilter::AllServices(vec![SERVICE, SERVICE2]))
            .await
            .unwrap(),
    );
    only_stress(
        scan(
            handler,
            ScanFilter::ManufacturerData(MANUFACTURER_ID, MANUFACTURER_DATA.to_vec()),
        )
        .await
        .unwrap(),
    );
    only_stress(
        scan(
            handler,
            ScanFilter::ManufacturerDataMasked(
                MANUFACTURER_ID,
                vec![0x21, 0x00, 0x00, 0x00],
                vec![0xff, 0x00, 0x00, 0x00],
            ),
        )
        .await
        .unwrap(),
    );

    assert!(scan(handler, ScanFilter::Service(CHARAC_MISSING))
        .await
        .unwrap()
        .is_empty());
    assert!(scan(
        handler,
        ScanFilter::ManufacturerData(MANUFACTURER_ID, vec![0x00, 0x00, 0x00, 0x00])
    )
    .await
    .unwrap()
    .is_empty());
    assert!(matches!(
        handler
            .discover(
                None,
                100,
                ScanFilter::ManufacturerDataMasked(MANUFACTURER_ID, vec![1], vec![1, 2]),
                false
            )
            .await,
        Err(crate::error::Error::InvalidFilterMask)
    ));
}

#[tokio::test(start_paused = true)]
async fn scanning_state_is_reported() {
    let (world, handler) = test_world();
    let _device = stress_device(&world);
    let mut updates = scanning_updates(handler).await;

    handler
        .discover(None, 400, ScanFilter::None, false)
        .await
        .unwrap();
    assert!(handler.is_scanning().await);
    assert!(world.is_scanning());
    assert_eq!(drain(&mut updates), vec![true]);

    assert!(wait_until(Duration::from_secs(5), || !world.is_scanning()).await);
    settle().await;
    assert!(!handler.is_scanning().await);
    assert_eq!(drain(&mut updates), vec![false]);
}

#[tokio::test(start_paused = true)]
async fn stop_scan_ends_scan_early() {
    let (world, handler) = test_world();
    let _device = stress_device(&world);
    let mut updates = scanning_updates(handler).await;

    handler
        .discover(None, 60_000, ScanFilter::None, false)
        .await
        .unwrap();
    settle().await;
    assert!(world.is_scanning());
    handler.stop_scan().await.unwrap();
    settle().await;
    assert!(!world.is_scanning());
    assert!(!handler.is_scanning().await);
    assert_eq!(drain(&mut updates), vec![true, false]);
}

#[tokio::test(start_paused = true)]
async fn second_scan_replaces_first() {
    let (world, handler) = test_world();
    let _device = stress_device(&world);

    handler
        .discover(None, 60_000, ScanFilter::None, false)
        .await
        .unwrap();
    settle().await;
    let devices = scan(handler, ScanFilter::None).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert!(!world.is_scanning());
    assert!(!handler.is_scanning().await);
}
