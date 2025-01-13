use async_trait::async_trait;
use btleplug::{
    api::{
        BDAddr, CentralEvent, CentralState, CharPropFlags, Characteristic, Descriptor,
        PeripheralProperties, Service, ValueNotification, WriteType,
    },
    platform::PeripheralId,
};
use futures::Stream;
use once_cell::sync::{Lazy, OnceCell};
use serde::Deserialize;
use std::{
    collections::{BTreeSet, HashMap},
    pin::Pin,
    vec,
};
use tauri::{
    ipc::{Channel, InvokeResponseBody},
    plugin::PluginHandle,
    AppHandle, Wry,
};
use tokio::sync::RwLock;
use tokio_stream::wrappers::ReceiverStream;
use tracing::info;
use uuid::Uuid;

type Result<T> = std::result::Result<T, btleplug::Error>;

static HANDLE: OnceCell<PluginHandle<Wry>> = OnceCell::new();

fn get_handle() -> &'static PluginHandle<Wry> {
    HANDLE.get().expect("plugin handle not initialized")
}

pub fn init<C: serde::de::DeserializeOwned>(
    _app: &AppHandle<Wry>,
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
            return Err(tauri::Error::from(e));
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

#[allow(dependency_on_unit_never_type_fallback)]
#[async_trait]
impl btleplug::api::Central for Adapter {
    type Peripheral = Peripheral;

    async fn events(&self) -> Result<Pin<Box<dyn Stream<Item = CentralEvent> + Send>>> {
        let (tx, rx) = tokio::sync::mpsc::channel::<CentralEvent>(1);
        let stream = ReceiverStream::new(rx);
        let channel: Channel = Channel::new(move |response| {
            let event = response
                .deserialize::<CentralEvent>()
                .expect("failed to deserialize event");
            tx.blocking_send(event)
                .expect("failed to send notification");
            Ok(())
        });
        get_handle()
            .run_mobile_plugin("events", channel)
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))?;
        Ok(Box::pin(stream))
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

#[allow(dependency_on_unit_never_type_fallback)]
#[async_trait]
impl btleplug::api::Manager for Manager {
    type Adapter = Adapter;

    async fn adapters(&self) -> Result<Vec<Adapter>> {
        Ok(vec![Adapter])
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Peripheral {
    id: PeripheralId,
    address: BDAddr,
    name: String,
    rssi: i16,
    #[serde(default)]
    manufacturer_data: HashMap<u16, Vec<u8>>,
    #[serde(default)]
    services: Vec<Uuid>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectParams {
    address: BDAddr,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MtuParams {
    address: BDAddr,
    mtu: u16,
}

#[derive(serde::Deserialize)]
struct BoolResult {
    result: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadParams {
    address: BDAddr,
    characteristic: Uuid,
}

#[allow(dependency_on_unit_never_type_fallback)]
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
            manufacturer_data: self.manufacturer_data.clone(),
            services: self.services.clone(),
            // TODO: implement the rest
            // at the moment not used by the handler or BleDevice struct so we can return default values
            address_type: Default::default(),
            class: Default::default(),
            tx_power_level: Default::default(),
            service_data: Default::default(),
        }))
    }

    fn services(&self) -> BTreeSet<Service> {
        #[derive(serde::Deserialize)]
        struct ResCharacteristic {
            uuid: Uuid,
            properties: u8,
            descriptors: Vec<Uuid>,
        }

        #[derive(serde::Deserialize)]
        struct ResService {
            uuid: Uuid,
            primary: bool,
            characs: Vec<ResCharacteristic>,
        }

        #[derive(serde::Deserialize)]
        struct ServicesResult {
            result: Vec<ResService>,
        }
        let res: ServicesResult = get_handle()
            .run_mobile_plugin(
                "services",
                ConnectParams {
                    address: self.address,
                },
            )
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))
            .expect("failed to get services");
        let mut services = BTreeSet::new();
        for s in res.result {
            let mut characteristics = BTreeSet::new();
            for c in s.characs {
                let mut descriptors = BTreeSet::new();
                for d in c.descriptors {
                    descriptors.insert(Descriptor {
                        uuid: d,
                        characteristic_uuid: c.uuid,
                        service_uuid: s.uuid,
                    });
                }
                characteristics.insert(Characteristic {
                    uuid: c.uuid,
                    service_uuid: s.uuid,
                    properties: CharPropFlags::from_bits_truncate(c.properties),
                    descriptors,
                });
            }
            services.insert(Service {
                uuid: s.uuid,
                primary: s.primary,
                characteristics,
            });
        }
        services
    }

    async fn is_connected(&self) -> Result<bool> {
        let res: BoolResult = get_handle()
            .run_mobile_plugin(
                "is_connected",
                ConnectParams {
                    address: self.address,
                },
            )
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))?;
        Ok(res.result)
    }

    async fn connect(&self) -> Result<()> {
        get_handle()
            .run_mobile_plugin(
                "connect",
                ConnectParams {
                    address: self.address,
                },
            )
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))?;
        get_handle()
            .run_mobile_plugin(
                "request_mtu",
                MtuParams {
                    address: self.address,
                    mtu: 512,
                },
            )
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))?;
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        get_handle()
            .run_mobile_plugin(
                "disconnect",
                ConnectParams {
                    address: self.address,
                },
            )
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))?;
        Ok(())
    }

    async fn discover_services(&self) -> Result<()> {
        get_handle()
            .run_mobile_plugin(
                "discover_services",
                ConnectParams {
                    address: self.address,
                },
            )
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))?;
        Ok(())
    }

    async fn write(
        &self,
        characteristic: &Characteristic,
        data: &[u8],
        write_type: WriteType,
    ) -> Result<()> {
        get_handle()
            .run_mobile_plugin(
                "write",
                serde_json::json!({
                    "address": self.address,
                    "characteristic": characteristic.uuid,
                    "data": data,
                    "withResponse": matches!(write_type, WriteType::WithResponse),
                }),
            )
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))?;
        Ok(())
    }

    async fn read(&self, characteristic: &Characteristic) -> Result<Vec<u8>> {
        #[derive(serde::Deserialize)]
        struct ReadResult {
            value: Vec<u8>,
        }
        let res: ReadResult = get_handle()
            .run_mobile_plugin(
                "read",
                ReadParams {
                    address: self.address,
                    characteristic: characteristic.uuid,
                },
            )
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))?;
        info!("read: {:?}", res.value);
        Ok(res.value)
    }

    async fn subscribe(&self, characteristic: &Characteristic) -> Result<()> {
        get_handle()
            .run_mobile_plugin(
                "subscribe",
                ReadParams {
                    address: self.address,
                    characteristic: characteristic.uuid,
                },
            )
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))?;
        Ok(())
    }

    async fn unsubscribe(&self, characteristic: &Characteristic) -> Result<()> {
        get_handle()
            .run_mobile_plugin(
                "unsubscribe",
                ReadParams {
                    address: self.address,
                    characteristic: characteristic.uuid,
                },
            )
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))?;
        Ok(())
    }

    async fn notifications(&self) -> Result<Pin<Box<dyn Stream<Item = ValueNotification> + Send>>> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Notification {
            uuid: Uuid,
            data: Vec<u8>,
        }
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct NotifyParams {
            address: BDAddr,
            channel: Channel<Notification>,
        }
        let (tx, rx) = tokio::sync::mpsc::channel::<ValueNotification>(1);
        let stream = ReceiverStream::new(rx);
        let channel: Channel<Notification> = Channel::new(move |response| {
            match response.deserialize::<Notification>() {
                Ok(notification) => tx
                    .blocking_send(ValueNotification {
                        uuid: notification.uuid,
                        value: notification.data,
                    })
                    .expect("failed to send notification"),
                Err(e) => {
                    tracing::error!("failed to deserialize notification: {:?}", e);
                    return Err(tauri::Error::from(e));
                }
            };
            Ok(())
        });
        get_handle()
            .run_mobile_plugin(
                "notifications",
                NotifyParams {
                    address: self.address,
                    channel,
                },
            )
            .map_err(|e| btleplug::Error::RuntimeError(e.to_string()))?;
        Ok(Box::pin(stream))
    }

    async fn write_descriptor(&self, _descriptor: &Descriptor, _data: &[u8]) -> Result<()> {
        todo!()
    }

    async fn read_descriptor(&self, _descriptor: &Descriptor) -> Result<Vec<u8>> {
        todo!()
    }
}
