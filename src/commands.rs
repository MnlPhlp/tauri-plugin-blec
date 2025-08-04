use tauri::ipc::Channel;
use tauri::{async_runtime, command, AppHandle, Runtime};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::error::Result;
use crate::get_handler;
use crate::models::{BleDevice, ScanFilter, WriteType};

#[command]
pub(crate) async fn scan<R: Runtime>(
    _app: AppHandle<R>,
    timeout: u64,
    on_devices: Channel<Vec<BleDevice>>,
    allow_ibeacons: bool,
) -> Result<()> {
    tracing::info!("Scanning for BLE devices");
    let handler = get_handler()?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    async_runtime::spawn(async move {
        while let Some(devices) = rx.recv().await {
            on_devices
                .send(devices)
                .expect("failed to send device to the front-end");
        }
    });
    handler
        .discover(Some(tx), timeout, ScanFilter::None, allow_ibeacons)
        .await?;
    Ok(())
}

#[command]
pub(crate) async fn stop_scan<R: Runtime>(_app: AppHandle<R>) -> Result<()> {
    tracing::info!("Stopping BLE scan");
    let handler = get_handler()?;
    handler.stop_scan().await?;
    Ok(())
}

#[command]
pub(crate) async fn connect<R: Runtime>(
    _app: AppHandle<R>,
    address: String,
    on_disconnect: Channel<()>,
    allow_ibeacons: bool,
) -> Result<()> {
    tracing::info!("Connecting to BLE device: {:?}", address);
    let handler = get_handler()?;
    let disconnct_handler = move || {
        on_disconnect
            .send(())
            .expect("failed to send disconnect event to the front-end");
    };
    handler
        .connect(&address, disconnct_handler.into(), allow_ibeacons)
        .await?;
    Ok(())
}

#[command]
pub(crate) async fn disconnect<R: Runtime>(_app: AppHandle<R>) -> Result<()> {
    tracing::info!("Disconnecting from BLE device");
    let handler = get_handler()?;
    handler.disconnect().await?;
    Ok(())
}

#[command]
pub(crate) async fn connection_state<R: Runtime>(
    _app: AppHandle<R>,
    update: Channel<bool>,
) -> Result<()> {
    let handler = get_handler()?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    handler.set_connection_update_channel(tx).await;
    update
        .send(handler.is_connected())
        .expect("failed to send connection state");
    async_runtime::spawn(async move {
        while let Some(connected) = rx.recv().await {
            debug!("Sending connection update to frontend: {connected}");
            update
                .send(connected)
                .expect("failed to send connection state to the front-end");
        }
        warn!("Connection state channel closed");
    });
    Ok(())
}

#[command]
pub(crate) async fn scanning_state<R: Runtime>(
    _app: AppHandle<R>,
    update: Channel<bool>,
) -> Result<()> {
    let handler = get_handler()?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    handler.set_scanning_update_channel(tx).await;
    update
        .send(handler.is_scanning().await)
        .expect("failed to send scanning state");
    async_runtime::spawn(async move {
        while let Some(scanning) = rx.recv().await {
            update
                .send(scanning)
                .expect("failed to send scanning state to the front-end");
        }
    });
    Ok(())
}

#[command]
pub(crate) async fn send<R: Runtime>(
    _app: AppHandle<R>,
    characteristic: Uuid,
    data: Vec<u8>,
    write_type: WriteType,
) -> Result<()> {
    info!("Sending data: {data:?}");
    let handler = get_handler()?;
    handler.send_data(characteristic, &data, write_type).await?;
    Ok(())
}

#[command]
pub(crate) async fn recv<R: Runtime>(_app: AppHandle<R>, characteristic: Uuid) -> Result<Vec<u8>> {
    let handler = get_handler()?;
    let data = handler.recv_data(characteristic).await?;
    Ok(data)
}

#[command]
pub(crate) async fn send_string<R: Runtime>(
    app: AppHandle<R>,
    characteristic: Uuid,
    data: String,
    write_type: WriteType,
) -> Result<()> {
    let data = data.as_bytes().to_vec();
    send(app, characteristic, data, write_type).await
}

#[command]
pub(crate) async fn recv_string<R: Runtime>(
    app: AppHandle<R>,
    characteristic: Uuid,
) -> Result<String> {
    let data = recv(app, characteristic).await?;
    Ok(String::from_utf8(data).expect("failed to convert data to string"))
}

async fn subscribe_channel(characteristic: Uuid) -> Result<mpsc::Receiver<Vec<u8>>> {
    let handler = get_handler()?;
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    handler
        .subscribe(characteristic, move |data: Vec<u8>| {
            info!("subscribe_channel: {:?}", data);
            tx.try_send(data)
                .expect("failed to send data to the channel");
        })
        .await?;
    Ok(rx)
}
#[command]
pub(crate) async fn subscribe<R: Runtime>(
    _app: AppHandle<R>,
    characteristic: Uuid,
    on_data: Channel<Vec<u8>>,
) -> Result<()> {
    let mut rx = subscribe_channel(characteristic).await?;
    async_runtime::spawn(async move {
        while let Some(data) = rx.recv().await {
            on_data
                .send(data)
                .expect("failed to send data to the front-end");
        }
    });
    Ok(())
}

#[command]
pub(crate) async fn subscribe_string<R: Runtime>(
    _app: AppHandle<R>,
    characteristic: Uuid,
    on_data: Channel<String>,
) -> Result<()> {
    let mut rx = subscribe_channel(characteristic).await?;
    async_runtime::spawn(async move {
        while let Some(data) = rx.recv().await {
            info!("subscribe_string: {:?}", data);
            let data = String::from_utf8(data).expect("failed to convert data to string");
            on_data
                .send(data)
                .expect("failed to send data to the front-end");
        }
    });
    Ok(())
}

#[command]
pub(crate) async fn unsubscribe<R: Runtime>(
    _app: AppHandle<R>,
    characteristic: Uuid,
) -> Result<()> {
    let handler = get_handler()?;
    handler.unsubscribe(characteristic).await?;
    Ok(())
}

#[command]
pub(crate) fn check_permissions() -> Result<bool> {
    crate::check_permissions()
}

pub fn commands<R: Runtime>() -> impl Fn(tauri::ipc::Invoke<R>) -> bool {
    tauri::generate_handler![
        scan,
        stop_scan,
        connect,
        disconnect,
        connection_state,
        send,
        send_string,
        recv,
        recv_string,
        subscribe,
        subscribe_string,
        unsubscribe,
        scanning_state,
        check_permissions
    ]
}
