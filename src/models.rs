use std::collections::HashMap;

use btleplug::api::BDAddr;
use enumflags2::BitFlags;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BleDevice {
    pub address: String,
    pub name: String,
    pub is_connected: bool,
    pub is_bonded: bool,
    pub manufacturer_data: HashMap<u16, Vec<u8>>,
    pub service_data: HashMap<Uuid, Vec<u8>>,
    pub services: Vec<Uuid>,
    pub rssi: Option<i16>,
    pub tx_power_level: Option<i16>,
}

impl Eq for BleDevice {}

impl PartialOrd for BleDevice {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BleDevice {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.address.cmp(&other.address)
    }
}

impl PartialEq for BleDevice {
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address
    }
}

#[async_trait::async_trait]
pub trait BondingPeripheral: btleplug::api::Peripheral {
    async fn is_bonded(&self) -> Result<bool, btleplug::Error> {
        Err(btleplug::Error::NotSupported(
            "Bonding is not implemented in btleplug".to_string(),
        ))
    }
}

// Default implementation for btleplug
#[cfg(not(target_os = "android"))]
impl<P: btleplug::api::Peripheral> BondingPeripheral for P {}

impl BleDevice {
    pub(crate) async fn from_peripheral<P: BondingPeripheral>(
        peripheral: &P,
    ) -> Result<Self, error::Error> {
        #[cfg(target_vendor = "apple")]
        let address = peripheral.id().to_string();
        #[cfg(not(target_vendor = "apple"))]
        let address = peripheral.address().to_string();
        let properties = peripheral.properties().await?.unwrap_or_default();
        let name = properties
            .local_name
            .unwrap_or_else(|| peripheral.id().to_string());
        Ok(Self {
            address,
            name,
            manufacturer_data: properties.manufacturer_data,
            service_data: properties.service_data,
            services: properties.services,
            rssi: properties.rssi,
            is_connected: peripheral.is_connected().await?,
            #[cfg(target_os = "android")]
            is_bonded: peripheral.is_bonded().await?,
            #[cfg(not(target_os = "android"))]
            is_bonded: false,
            tx_power_level: properties.tx_power_level,
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Service {
    pub uuid: Uuid,
    pub characteristics: Vec<Characteristic>,
}

impl From<&btleplug::api::Service> for Service {
    fn from(service: &btleplug::api::Service) -> Self {
        Self {
            uuid: service.uuid,
            characteristics: service
                .characteristics
                .iter()
                .map(Characteristic::from)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Characteristic {
    pub uuid: Uuid,
    pub descriptors: Vec<Uuid>,
    pub properties: BitFlags<CharProps>,
}

impl From<&btleplug::api::Characteristic> for Characteristic {
    fn from(characteristic: &btleplug::api::Characteristic) -> Self {
        Self {
            uuid: characteristic.uuid,
            descriptors: characteristic.descriptors.iter().map(|d| d.uuid).collect(),
            properties: get_flags(characteristic.properties),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[enumflags2::bitflags]
#[repr(u8)]
pub enum CharProps {
    Broadcast,
    Read,
    WriteWithoutResponse,
    Write,
    Notify,
    Indicate,
    AuthenticatedSignedWrites,
    ExtendedProperties,
}

impl From<btleplug::api::CharPropFlags> for CharProps {
    fn from(flag: btleplug::api::CharPropFlags) -> Self {
        match flag {
            btleplug::api::CharPropFlags::BROADCAST => CharProps::Broadcast,
            btleplug::api::CharPropFlags::READ => CharProps::Read,
            btleplug::api::CharPropFlags::WRITE_WITHOUT_RESPONSE => CharProps::WriteWithoutResponse,
            btleplug::api::CharPropFlags::WRITE => CharProps::Write,
            btleplug::api::CharPropFlags::NOTIFY => CharProps::Notify,
            btleplug::api::CharPropFlags::INDICATE => CharProps::Indicate,
            btleplug::api::CharPropFlags::AUTHENTICATED_SIGNED_WRITES => {
                CharProps::AuthenticatedSignedWrites
            }
            btleplug::api::CharPropFlags::EXTENDED_PROPERTIES => CharProps::ExtendedProperties,
            _ => unreachable!(),
        }
    }
}

fn get_flags(properties: btleplug::api::CharPropFlags) -> BitFlags<CharProps, u8> {
    let mut flags = BitFlags::empty();
    for flag in properties.iter() {
        flags |= CharProps::from(flag);
    }
    flags
}

#[must_use]
pub fn fmt_addr(addr: BDAddr) -> String {
    let a = addr.into_inner();
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        a[0], a[1], a[2], a[3], a[4], a[5]
    )
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WriteType {
    /// aka request.
    WithResponse,
    /// aka command.
    WithoutResponse,
}

impl From<WriteType> for btleplug::api::WriteType {
    fn from(write_type: WriteType) -> Self {
        match write_type {
            WriteType::WithResponse => btleplug::api::WriteType::WithResponse,
            WriteType::WithoutResponse => btleplug::api::WriteType::WithoutResponse,
        }
    }
}

/// Filter for discovering devices.
/// Only devices matching the filter will be returned by the `handler::discover` method
pub enum ScanFilter {
    None,
    /// Matches if the device advertises the specified service.
    Service(Uuid),
    /// Matches if the device advertises any of the specified services.
    AnyService(Vec<Uuid>),
    /// Matches if the device advertises all of the specified services.
    AllServices(Vec<Uuid>),
    /// Matches if the device advertises the specified manufacturer data.
    ManufacturerData(u16, Vec<u8>),
    /// Matches if the device advertises the specified manufacturer data, checking only the bits
    /// that are 1 in the mask
    ManufacturerDataMasked(u16, Vec<u8>, Vec<u8>),
}

/// State of the Bluetooth adapter
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum AdapterState {
    Unknown,
    Off,
    On,
}
