use tauri::ipc::Channel;
use tauri::{async_runtime, command, AppHandle, Runtime};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::error::Result;
use crate::get_handler;
use crate::models::{AdapterState, BleDevice, ScanFilter, Service, WriteType};

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
            if let Err(e) = on_devices.send(devices) {
                warn!("Failed to send devices to the front-end: {e}");
                return;
            }
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
        if let Err(e) = on_disconnect.send(()) {
            warn!("Failed to send disconnect event to the front-end: {e}");
        }
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
    if let Err(e) = update.send(handler.is_connected()) {
        warn!("Failed to send connection state to the front-end: {e}");
    }
    async_runtime::spawn(async move {
        while let Some(connected) = rx.recv().await {
            if let Err(e) = update.send(connected) {
                warn!("Failed to send connection state to the front-end: {e}");
                return;
            }
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
    if let Err(e) = update.send(handler.is_scanning().await) {
        warn!("failed to send scanning state to the front-end: {e}");
    }
    async_runtime::spawn(async move {
        while let Some(scanning) = rx.recv().await {
            if let Err(e) = update.send(scanning) {
                warn!("failed to send scanning state to the front-end: {e}");
                return;
            }
        }
    });
    Ok(())
}

#[command]
pub(crate) async fn send<R: Runtime>(
    _app: AppHandle<R>,
    characteristic: Uuid,
    service: Option<Uuid>,
    data: Vec<u8>,
    write_type: WriteType,
) -> Result<()> {
    info!("Sending data: {data:?}");
    let handler = get_handler()?;
    handler
        .send_data(characteristic, service, &data, write_type)
        .await?;
    Ok(())
}

#[command]
pub(crate) async fn recv<R: Runtime>(
    _app: AppHandle<R>,
    characteristic: Uuid,
    service: Option<Uuid>,
) -> Result<Vec<u8>> {
    let handler = get_handler()?;
    let data = handler.recv_data(characteristic, service).await?;
    Ok(data)
}

#[command]
pub(crate) async fn send_string<R: Runtime>(
    app: AppHandle<R>,
    characteristic: Uuid,
    service: Option<Uuid>,
    data: String,
    write_type: WriteType,
) -> Result<()> {
    let data = data.as_bytes().to_vec();
    send(app, characteristic, service, data, write_type).await
}

#[command]
pub(crate) async fn recv_string<R: Runtime>(
    app: AppHandle<R>,
    characteristic: Uuid,
    service: Option<Uuid>,
) -> Result<String> {
    let data = recv(app, characteristic, service).await?;
    Ok(String::from_utf8(data).expect("failed to convert data to string"))
}

async fn subscribe_channel(
    characteristic: Uuid,
    service: Option<Uuid>,
) -> Result<mpsc::Receiver<Vec<u8>>> {
    let handler = get_handler()?;
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    handler
        .subscribe(characteristic, service, move |data: Vec<u8>| {
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
    service: Option<Uuid>,
    on_data: Channel<Vec<u8>>,
) -> Result<()> {
    let mut rx = subscribe_channel(characteristic, service).await?;
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
    service: Option<Uuid>,
    on_data: Channel<String>,
) -> Result<()> {
    let mut rx = subscribe_channel(characteristic, service).await?;
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
pub(crate) fn check_permissions(
    _app: AppHandle<impl Runtime>,
    ask_if_denied: bool,
) -> Result<bool> {
    crate::check_permissions(ask_if_denied)
}

#[command]
pub(crate) async fn list_services<R: Runtime>(
    _app: tauri::AppHandle<R>,
    address: String,
) -> Result<Vec<Service>> {
    let handler = get_handler()?;
    let services = handler
        .discover_services(&address)
        .await
        .expect("Unable to discover services");
    Ok(services)
}

#[command]
pub(crate) async fn get_adapter_state<R: Runtime>(_app: AppHandle<R>) -> Result<AdapterState> {
    let handler = get_handler()?;
    let state = handler.get_adapter_state().await;
    Ok(state)
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
        check_permissions,
        list_services,
        get_adapter_state
    ]
}
