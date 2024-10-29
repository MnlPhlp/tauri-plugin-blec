use async_trait::async_trait;
use btleplug::{
    api::{
        BDAddr, CentralEvent, CentralState, Characteristic, Descriptor, PeripheralProperties,
        Service, ValueNotification, WriteType,
    },
    platform::PeripheralId,
};
use futures::{stream::Once, Stream};
use once_cell::sync::{Lazy, OnceCell};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    fmt::Display,
    pin::Pin,
    vec,
};
use tauri::{
    ipc::{Channel, InvokeResponseBody},
    plugin::PluginHandle,
    AppHandle, Manager as _, Wry,
};
use tokio::sync::RwLock;
use uuid::Uuid;

type Result<T> = std::result::Result<T, btleplug::Error>;

static HANDLE: OnceCell<PluginHandle<Wry>> = OnceCell::new();

fn get_handle() -> &'static PluginHandle<Wry> {
    HANDLE.get().expect("plugin handle not initialized")
}

pub fn init<C: serde::de::DeserializeOwned>(
    app: &AppHandle<Wry>,
    api: tauri::plugin::PluginApi<Wry, C>,
) -> std::result::Result<(), crate::error::Error> {
    let handle = api.register_android_plugin("com.plugin.blec", "BleClientPlugin")?;
    HANDLE.set(handle).unwrap();
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Adapter;
static DEVICES: Lazy<RwLock<HashMap<PeripheralId, Peripheral>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

#[derive(serde::Deserialize)]
struct PeripheralResult {
    result: Peripheral,
}

fn on_device_callback(response: InvokeResponseBody) -> std::result::Result<(), tauri::Error> {
    let device = match response.deserialize::<PeripheralResult>() {
        Ok(PeripheralResult { result }) => result,
        Err(e) => {
            tracing::error!("failed to deserialize peripheral: {:?}", e);
            return Ok(());
        }
    };
    let mut devices = DEVICES.blocking_write();
    tracing::info!("device: {device:?}");
    if let Some(enty) = devices.get_mut(&device.id) {
        *enty = device;
    } else {
        devices.insert(device.id.clone(), device);
    }
    Ok(())
}

#[async_trait]
impl btleplug::api::Central for Adapter {
    type Peripheral = Peripheral;

    async fn events(&self) -> Result<Pin<Box<dyn Stream<Item = CentralEvent> + Send>>> {
        todo!()
    }

    async fn start_scan(&self, filter: btleplug::api::ScanFilter) -> Result<()> {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct ScanParams {
            services: Vec<Uuid>,
            on_device: Channel<serde_json::Value>,
        }
        DEVICES.write().await.clear();
        let on_device = Channel::new(on_device_callback);
        get_handle()
            .run_mobile_plugin(
                "start_scan",
                ScanParams {
                    services: filter.services,
                    on_device,
                },
            )
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))?;
        Ok(())
    }

    async fn stop_scan(&self) -> Result<()> {
        get_handle()
            .run_mobile_plugin("stop_scan", serde_json::Value::Null)
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))?;
        Ok(())
    }

    async fn peripherals(&self) -> Result<Vec<Self::Peripheral>> {
        Ok(DEVICES.read().await.values().cloned().collect())
    }

    async fn peripheral(&self, id: &PeripheralId) -> Result<Self::Peripheral> {
        DEVICES
            .read()
            .await
            .get(&id)
            .cloned()
            .ok_or(btleplug::Error::DeviceNotFound)
    }

    async fn add_peripheral(&self, _address: &PeripheralId) -> Result<Self::Peripheral> {
        Err(btleplug::Error::NotSupported("add_peripheral".to_string()))
    }

    async fn adapter_info(&self) -> Result<String> {
        todo!()
    }

    async fn adapter_state(&self) -> Result<CentralState> {
        todo!()
    }
}

pub struct Manager;

impl Manager {
    pub async fn new() -> Result<Self> {
        Ok(Manager)
    }
}

#[async_trait]
impl btleplug::api::Manager for Manager {
    type Adapter = Adapter;

    async fn adapters(&self) -> Result<Vec<Adapter>> {
        Ok(vec![Adapter])
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Peripheral {
    id: PeripheralId,
    address: BDAddr,
    name: String,
    rssi: i16,
    connected: bool,
}

#[async_trait::async_trait]
impl btleplug::api::Peripheral for Peripheral {
    fn id(&self) -> PeripheralId {
        self.id.clone()
    }

    fn address(&self) -> BDAddr {
        self.address
    }

    async fn properties(&self) -> Result<Option<PeripheralProperties>> {
        Ok(Some(PeripheralProperties {
            address: self.address,
            local_name: Some(self.name.clone()),
            rssi: Some(self.rssi),
            ..Default::default()
        }))
    }

    fn services(&self) -> BTreeSet<Service> {
        todo!()
    }

    async fn is_connected(&self) -> Result<bool> {
        Ok(self.connected)
    }

    async fn connect(&self) -> Result<()> {
        todo!()
    }

    async fn disconnect(&self) -> Result<()> {
        todo!()
    }

    async fn discover_services(&self) -> Result<()> {
        todo!()
    }

    async fn write(
        &self,
        characteristic: &Characteristic,
        data: &[u8],
        write_type: WriteType,
    ) -> Result<()> {
        todo!()
    }

    async fn read(&self, characteristic: &Characteristic) -> Result<Vec<u8>> {
        todo!()
    }

    async fn subscribe(&self, characteristic: &Characteristic) -> Result<()> {
        todo!()
    }

    async fn unsubscribe(&self, characteristic: &Characteristic) -> Result<()> {
        todo!()
    }

    async fn notifications(&self) -> Result<Pin<Box<dyn Stream<Item = ValueNotification> + Send>>> {
        todo!()
    }

    async fn write_descriptor(&self, descriptor: &Descriptor, data: &[u8]) -> Result<()> {
        todo!()
    }

    async fn read_descriptor(&self, descriptor: &Descriptor) -> Result<Vec<u8>> {
        todo!()
    }
}