//! Construction of the platform specific `PeripheralId` for mock devices.
//!
//! `btleplug::platform::PeripheralId` has no public constructor, but with the
//! `serde` feature it can be deserialised. Its representation differs per
//! backend, so the mock builds the matching JSON value for the target it is
//! compiled for.

use btleplug::api::BDAddr;
use btleplug::platform::PeripheralId;

/// Returns the `PeripheralId` a device with `address` would have on this platform.
///
/// # Panics
/// Panics if the representation assumed for this platform does not match btleplug,
/// which would be a bug in this module.
#[must_use]
pub fn peripheral_id(address: BDAddr) -> PeripheralId {
    #[cfg(target_vendor = "apple")]
    {
        // CoreBluetooth identifies peripherals by a UUID; derive a stable one
        // from the address so ids stay comparable across calls.
        let mut bytes = [0u8; 16];
        bytes[10..].copy_from_slice(&address.into_inner());
        PeripheralId::from(uuid::Uuid::from_bytes(bytes))
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        serde_json::from_value(id_value(address))
            .expect("mock PeripheralId representation matches btleplug")
    }
}

/// BlueZ: the D-Bus object path of the device.
#[cfg(target_os = "linux")]
fn id_value(address: BDAddr) -> serde_json::Value {
    let path = format!(
        "/org/bluez/hci0/dev_{}",
        address.to_string().replace(':', "_")
    );
    serde_json::json!({ "object_path": path })
}

/// WinRT and the plugin's Android backend: the colon delimited address.
#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
fn id_value(address: BDAddr) -> serde_json::Value {
    serde_json::Value::String(address.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_stable_and_unique() {
        let a = BDAddr::from([1, 2, 3, 4, 5, 6]);
        let b = BDAddr::from([1, 2, 3, 4, 5, 7]);
        assert_eq!(peripheral_id(a), peripheral_id(a));
        assert_ne!(peripheral_id(a), peripheral_id(b));
    }
}
