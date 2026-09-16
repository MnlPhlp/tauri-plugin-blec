//! The android BLE backend.
//!
//! Implements the btleplug traits the [`Handler`](crate::Handler) uses on top of
//! the Kotlin side, which it reaches through the JNI [`bridge`].

mod bridge;

pub use bridge::{init, set_activity};

use crate::ALLOW_IBEACONS;
use async_trait::async_trait;
use base64::Engine;
use btleplug::{
    api::{
        BDAddr, CentralEvent, CentralState, CharPropFlags, Characteristic, Descriptor,
        PeripheralProperties, Service, ValueNotification, WriteType,
    },
    platform::PeripheralId,
};
use futures::{Stream, StreamExt};
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::RwLock as StdRwLock;
use std::time::Duration;
use std::{
    collections::{BTreeSet, HashMap},
    pin::Pin,
    vec,
};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::error::Error;
use crate::models::BondingPeripheral;
use crate::Handler;

type Result<T> = std::result::Result<T, btleplug::Error>;

/// Timeout for the commands that only read state the Kotlin side already has.
/// Everything that talks to the radio uses the matching [`crate::Timeouts`].
const IPC_DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

pub static REQUESTED_MTU: AtomicU16 = AtomicU16::new(517);

fn ble_err(e: Error) -> btleplug::Error {
    btleplug::Error::RuntimeError(e.to_string())
}

/// The timeouts configured on the handler, or the defaults while it is not
/// initialized yet.
fn timeouts() -> crate::models::Timeouts {
    crate::get_handler().map_or_else(|_| crate::models::Timeouts::default(), Handler::timeouts)
}

/// Checks if the app has the necessary permissions to use BLE, asking for them
/// if they are missing.
///
/// If they were denied before, android does not show the dialog again, so with
/// `ask_if_denied` the user is sent to the app's settings page instead.
pub(crate) async fn check_permissions(ask_if_denied: bool) -> std::result::Result<bool, Error> {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct CheckPermissionsParams {
        ask_if_denied: bool,
        allow_ibeacons: bool,
    }
    // Permission requests wait for the user, so there is no useful timeout here
    // beyond "the activity never came back".
    let res: BoolResult = bridge::call(
        "check_permissions",
        CheckPermissionsParams {
            ask_if_denied,
            allow_ibeacons: ALLOW_IBEACONS.load(Ordering::Relaxed),
        },
        Duration::from_secs(300),
    )
    .await?;
    Ok(res.result)
}

#[derive(Debug, Clone)]
pub struct Adapter;
static DEVICES: Lazy<RwLock<HashMap<PeripheralId, Peripheral>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// The services discovered per peripheral.
///
/// `btleplug::api::Peripheral::services()` is sync, so it cannot ask the Kotlin
/// side; `discover_services` fills this in and `services` reads it back. A
/// connect or disconnect drops the entry, because the services belong to the
/// GATT link that just went away.
static SERVICES: Lazy<StdRwLock<HashMap<PeripheralId, BTreeSet<Service>>>> =
    Lazy::new(|| StdRwLock::new(HashMap::new()));

/// The task forwarding scan results into [`DEVICES`], one per running scan.
static SCAN_TASK: Lazy<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
    Lazy::new(|| std::sync::Mutex::new(None));

#[derive(serde::Deserialize)]
struct PeripheralResult {
    result: Peripheral,
}

#[allow(dependency_on_unit_never_type_fallback)]
#[async_trait]
impl btleplug::api::Central for Adapter {
    type Peripheral = Peripheral;

    async fn clear_peripherals(&self) -> Result<()> {
        DEVICES.write().await.clear();
        bridge::call::<_, ()>(
            "clear_peripherals",
            serde_json::Value::Null,
            IPC_DEFAULT_TIMEOUT,
        )
        .await
        .map_err(ble_err)?;
        Ok(())
    }

    async fn events(&self) -> Result<Pin<Box<dyn Stream<Item = CentralEvent> + Send>>> {
        #[derive(serde::Serialize)]
        struct EventParams {
            channel: bridge::Channel,
        }
        let (channel, rx) = bridge::channel().map_err(ble_err)?;
        bridge::call::<_, ()>("events", EventParams { channel }, IPC_DEFAULT_TIMEOUT)
            .await
            .map_err(ble_err)?;
        let stream = rx.filter_map(|value| async move {
            // Diagnostic from the Kotlin side, not a btleplug event: our
            // BluetoothGatt is gone but the phone keeps the radio link for
            // another GATT client, so the peripheral never sees a disconnect
            // and the next connect silently reuses that link.
            if let Some(address) = value.get("LinkStillConnected") {
                warn!(
                    "disconnected from {address}, but the phone still holds a GATT link to it \
                     (another app or a leaked BluetoothGatt): the device will not notice the disconnect"
                );
                return None;
            }
            match serde_json::from_value::<CentralEvent>(value) {
                Ok(event) => {
                    debug!("sending event: {event:?}");
                    Some(event)
                }
                Err(e) => {
                    tracing::error!("failed to deserialize event: {e}");
                    None
                }
            }
        });
        Ok(Box::pin(stream))
    }

    async fn start_scan(&self, filter: btleplug::api::ScanFilter) -> Result<()> {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct ScanParams {
            services: Vec<Uuid>,
            allow_ibeacons: bool,
            on_device: bridge::Channel,
        }
        DEVICES.write().await.clear();
        let (on_device, mut rx) = bridge::channel().map_err(ble_err)?;
        let task = tokio::spawn(async move {
            while let Some(value) = rx.next().await {
                let device = match serde_json::from_value::<PeripheralResult>(value) {
                    Ok(PeripheralResult { result }) => result,
                    Err(e) => {
                        tracing::error!("failed to deserialize peripheral: {e}");
                        continue;
                    }
                };
                tracing::trace!("device: {device:?}");
                DEVICES.write().await.insert(device.id.clone(), device);
            }
        });
        // A scan that is still running keeps its own channel registered; replace
        // it so only the new scan's results reach `DEVICES`.
        if let Some(previous) = SCAN_TASK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(task)
        {
            previous.abort();
        }
        bridge::call::<_, ()>(
            "start_scan",
            ScanParams {
                services: filter.services,
                allow_ibeacons: ALLOW_IBEACONS.load(Ordering::Relaxed),
                on_device,
            },
            IPC_DEFAULT_TIMEOUT,
        )
        .await
        .map_err(ble_err)?;
        Ok(())
    }

    async fn stop_scan(&self) -> Result<()> {
        bridge::call::<_, ()>("stop_scan", serde_json::Value::Null, IPC_DEFAULT_TIMEOUT)
            .await
            .map_err(ble_err)?;
        if let Some(task) = SCAN_TASK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
        Ok(())
    }

    async fn peripherals(&self) -> Result<Vec<Self::Peripheral>> {
        Ok(DEVICES.read().await.values().cloned().collect())
    }

    /// Resolves known addresses without a scan, so a peripheral that was lost
    /// when the device map was cleared can be used again. Only the identifier
    /// selector is supported: `BluetoothAdapter.getRemoteDevice` takes a MAC,
    /// and android has no equivalent of retrieving connected peripherals by
    /// service.
    async fn retrieve_peripherals(
        &self,
        options: btleplug::api::RetrievePeripheralsOptions,
    ) -> Result<Vec<Self::Peripheral>> {
        let Some(identifiers) = options.identifiers.filter(|_| options.services.is_none()) else {
            return Err(btleplug::Error::NotSupported(
                "retrieve_peripherals".to_string(),
            ));
        };
        let mut peripherals = Vec::with_capacity(identifiers.len());
        for id in identifiers {
            // droidplug's PeripheralId is the MAC, but it does not hand out the
            // BDAddr it wraps
            let address = id
                .to_string()
                .parse()
                .map_err(|_| btleplug::Error::DeviceNotFound)?;
            let res: PeripheralResult = bridge::call(
                "retrieve_peripheral",
                ConnectParams { address },
                IPC_DEFAULT_TIMEOUT,
            )
            .await
            .map_err(ble_err)?;
            // The java side answers from `BluetoothDevice`, which knows nothing
            // about advertisements, so an entry a scan already filled in is
            // kept: the point of the call is repairing the java device map.
            let mut devices = DEVICES.write().await;
            let peripheral = devices
                .entry(res.result.id.clone())
                .or_insert(res.result)
                .clone();
            peripherals.push(peripheral);
        }
        Ok(peripherals)
    }

    async fn peripheral(&self, id: &PeripheralId) -> Result<Self::Peripheral> {
        DEVICES
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or(btleplug::Error::DeviceNotFound)
    }

    async fn add_peripheral(&self, _address: &PeripheralId) -> Result<Self::Peripheral> {
        Err(btleplug::Error::NotSupported("add_peripheral".to_string()))
    }

    async fn adapter_info(&self) -> Result<String> {
        Ok("android".to_string())
    }

    async fn adapter_state(&self) -> Result<CentralState> {
        let res: StringResult = bridge::call(
            "adapter_state",
            serde_json::Value::Null,
            IPC_DEFAULT_TIMEOUT,
        )
        .await
        .map_err(ble_err)?;
        match res.result.as_str() {
            "unknown" => Ok(CentralState::Unknown),
            "off" => Ok(CentralState::PoweredOff),
            "on" => Ok(CentralState::PoweredOn),
            _ => Err(btleplug::Error::RuntimeError(format!(
                "unknown adapter state: {}",
                res.result
            ))),
        }
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

fn deserialize_base64<'a, D>(deserializer: D) -> std::result::Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'a>,
{
    let s = String::deserialize(deserializer)?;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(serde::de::Error::custom)
}

fn deserialize_base64_map<'a, D, K>(
    deserializer: D,
) -> std::result::Result<HashMap<K, Vec<u8>>, D::Error>
where
    D: serde::Deserializer<'a>,
    K: serde::Deserialize<'a> + std::hash::Hash + std::cmp::Eq,
{
    let map: HashMap<K, String> = serde::Deserialize::deserialize(deserializer)?;
    let mut res = HashMap::new();
    for (k, v) in map {
        res.insert(
            k,
            base64::engine::general_purpose::STANDARD
                .decode(v)
                .map_err(serde::de::Error::custom)?,
        );
    }
    Ok(res)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Peripheral {
    id: PeripheralId,
    address: BDAddr,
    name: String,
    rssi: i16,
    #[serde(default = "default_mtu")]
    mtu_val: AtomicU16,
    #[serde(default, deserialize_with = "deserialize_base64_map")]
    manufacturer_data: HashMap<u16, Vec<u8>>,
    #[serde(default, deserialize_with = "deserialize_base64_map")]
    service_data: HashMap<Uuid, Vec<u8>>,
    #[serde(default)]
    services: Vec<Uuid>,
    tx_power_level: Option<i16>,
}
fn default_mtu() -> AtomicU16 {
    AtomicU16::new(23)
}
impl Clone for Peripheral {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            address: self.address,
            name: self.name.clone(),
            rssi: self.rssi,
            mtu_val: AtomicU16::new(self.mtu_val.load(Ordering::Relaxed)),
            manufacturer_data: self.manufacturer_data.clone(),
            service_data: self.service_data.clone(),
            services: self.services.clone(),
            tx_power_level: self.tx_power_level,
        }
    }
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
#[serde(rename_all = "camelCase")]
struct MtuResponse {
    mtu: u16,
}

#[derive(serde::Deserialize)]
struct BoolResult {
    result: bool,
}

#[derive(serde::Deserialize)]
struct StringResult {
    result: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadParams {
    address: BDAddr,
    characteristic: Uuid,
    service: Uuid,
}

#[async_trait::async_trait]
impl BondingPeripheral for Peripheral {
    async fn is_bonded(&self) -> Result<bool> {
        let res: BoolResult = bridge::call(
            "is_bonded",
            ConnectParams {
                address: self.address,
            },
            IPC_DEFAULT_TIMEOUT,
        )
        .await
        .map_err(ble_err)?;
        Ok(res.result)
    }
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

    fn mtu(&self) -> u16 {
        self.mtu_val.load(Ordering::Relaxed)
    }

    async fn properties(&self) -> Result<Option<PeripheralProperties>> {
        Ok(Some(PeripheralProperties {
            address: self.address,
            local_name: Some(self.name.clone()),
            advertisement_name: Some(self.name.clone()),
            rssi: Some(self.rssi),
            manufacturer_data: self.manufacturer_data.clone(),
            service_data: self.service_data.clone(),
            services: self.services.clone(),
            tx_power_level: self.tx_power_level,
            // TODO: implement the rest
            // at the moment not used by the handler or BleDevice struct so we can return default values
            address_type: Default::default(),
            class: Default::default(),
            appearance: None,
        }))
    }

    /// The services [`Self::discover_services`] found, empty before it ran.
    fn services(&self) -> BTreeSet<Service> {
        SERVICES
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.id)
            .cloned()
            .unwrap_or_default()
    }

    async fn is_connected(&self) -> Result<bool> {
        let res: BoolResult = bridge::call(
            "is_connected",
            ConnectParams {
                address: self.address,
            },
            IPC_DEFAULT_TIMEOUT,
        )
        .await
        .map_err(ble_err)?;
        Ok(res.result)
    }

    async fn connect(&self) -> Result<()> {
        let timeouts = timeouts();
        self.forget_services();
        bridge::call::<_, ()>(
            "connect",
            ConnectParams {
                address: self.address,
            },
            timeouts.connect,
        )
        .await
        .map_err(ble_err)?;
        info!("connected to: {:?}", self.address);
        let requested_mtu = REQUESTED_MTU.load(Ordering::Relaxed);
        if requested_mtu > 0 {
            debug!("requesting mtu");
            let mtu: MtuResponse = bridge::call(
                "request_mtu",
                MtuParams {
                    address: self.address,
                    mtu: requested_mtu,
                },
                timeouts.connect,
            )
            .await
            .map_err(ble_err)?;
            info!("mtu set to: {:?}", mtu.mtu);
            self.mtu_val.store(mtu.mtu, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        self.forget_services();
        bridge::call::<_, ()>(
            "disconnect",
            ConnectParams {
                address: self.address,
            },
            timeouts().disconnect,
        )
        .await
        .map_err(ble_err)?;
        Ok(())
    }

    async fn discover_services(&self) -> Result<()> {
        bridge::call::<_, ()>(
            "discover_services",
            ConnectParams {
                address: self.address,
            },
            timeouts().discover_services,
        )
        .await
        .map_err(ble_err)?;
        debug!("discover services plugin call returned");
        self.fetch_services().await?;
        Ok(())
    }

    async fn write(
        &self,
        characteristic: &Characteristic,
        data: &[u8],
        write_type: WriteType,
    ) -> Result<()> {
        let (timeout, skip_waiting_for_completion) = crate::get_handler()
            .map(|handler| handler.get_write_behaviour())
            .unwrap_or((0, false));
        let write_timeout = timeouts().write;
        // 0 means "no timeout" on the Kotlin side, which would leave a queued
        // write pending forever. Fall back to the handler timeout so the queue
        // expiry always exists and fires before the Rust side gives up.
        let timeout = if timeout == 0 {
            u32::try_from(write_timeout.as_millis()).unwrap_or(u32::MAX)
        } else {
            timeout
        };
        bridge::call::<_, ()>(
            "write",
            serde_json::json!({
                "address": self.address,
                "characteristic": characteristic.uuid,
                "service": characteristic.service_uuid,
                "data": data,
                "withResponse": matches!(write_type, WriteType::WithResponse),
                "timeout": timeout,
                "skipWaitingForWriteToComplete": skip_waiting_for_completion
            }),
            write_timeout,
        )
        .await
        .map_err(ble_err)?;
        Ok(())
    }

    async fn read(&self, characteristic: &Characteristic) -> Result<Vec<u8>> {
        #[derive(serde::Deserialize)]
        struct ReadResult {
            #[serde(deserialize_with = "deserialize_base64")]
            value: Vec<u8>,
        }
        let res: ReadResult = bridge::call(
            "read",
            ReadParams {
                address: self.address,
                characteristic: characteristic.uuid,
                service: characteristic.service_uuid,
            },
            timeouts().read,
        )
        .await
        .map_err(ble_err)?;
        debug!("read: {:?}", res.value);
        Ok(res.value)
    }

    async fn subscribe(&self, characteristic: &Characteristic) -> Result<()> {
        bridge::call::<_, ()>(
            "subscribe",
            ReadParams {
                address: self.address,
                characteristic: characteristic.uuid,
                service: characteristic.service_uuid,
            },
            timeouts().subscribe,
        )
        .await
        .map_err(ble_err)?;
        Ok(())
    }

    async fn unsubscribe(&self, characteristic: &Characteristic) -> Result<()> {
        bridge::call::<_, ()>(
            "unsubscribe",
            ReadParams {
                address: self.address,
                characteristic: characteristic.uuid,
                service: characteristic.service_uuid,
            },
            timeouts().subscribe,
        )
        .await
        .map_err(ble_err)?;
        Ok(())
    }

    async fn notifications(&self) -> Result<Pin<Box<dyn Stream<Item = ValueNotification> + Send>>> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Notification {
            uuid: Uuid,
            service_uuid: Uuid,
            #[serde(deserialize_with = "deserialize_base64")]
            data: Vec<u8>,
        }
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct NotifyParams {
            address: BDAddr,
            channel: bridge::Channel,
        }
        let (channel, rx) = bridge::channel().map_err(ble_err)?;
        bridge::call::<_, ()>(
            "notifications",
            NotifyParams {
                address: self.address,
                channel,
            },
            IPC_DEFAULT_TIMEOUT,
        )
        .await
        .map_err(ble_err)?;
        let stream = rx.filter_map(|value| async move {
            match serde_json::from_value::<Notification>(value) {
                Ok(notification) => Some(ValueNotification {
                    uuid: notification.uuid,
                    service_uuid: notification.service_uuid,
                    value: notification.data,
                }),
                Err(e) => {
                    tracing::error!("failed to deserialize notification: {e}");
                    None
                }
            }
        });
        Ok(Box::pin(stream))
    }

    async fn write_descriptor(&self, _descriptor: &Descriptor, _data: &[u8]) -> Result<()> {
        Err(btleplug::Error::NotSupported(
            "write_descriptor".to_string(),
        ))
    }

    async fn read_descriptor(&self, _descriptor: &Descriptor) -> Result<Vec<u8>> {
        Err(btleplug::Error::NotSupported("read_descriptor".to_string()))
    }
}

impl Peripheral {
    /// Reads the discovered services from the Kotlin side into [`SERVICES`].
    async fn fetch_services(&self) -> Result<()> {
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

        let res: ServicesResult = bridge::call(
            "services",
            ConnectParams {
                address: self.address,
            },
            IPC_DEFAULT_TIMEOUT,
        )
        .await
        .map_err(ble_err)?;
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
        SERVICES
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.id.clone(), services);
        Ok(())
    }

    fn forget_services(&self) {
        SERVICES
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}
