use serde::{Serialize, Serializer};
use tokio::sync::mpsc::error::SendError;

use thiserror::Error;
use uuid::Uuid;
#[derive(Debug, Error)]
pub enum Error {
    #[error("Btleplug error: {0}")]
    Btleplug(#[from] btleplug::Error),

    #[error("There is no peripheral with id: {0}")]
    UnknownPeripheral(String),

    #[error("Characteristic {0} not available")]
    CharacNotAvailable(String),

    /// More than one characteristic matched. GATT allows the same UUID in
    /// several services, and even several times in one service.
    #[error(
        "Characteristic {uuid} is ambiguous, it exists {} times (services: {services:?}); \
        pass the service. Duplicate characteristics within one service are not supported",
        services.len()
    )]
    AmbiguousCharacteristic { uuid: Uuid, services: Vec<Uuid> },

    #[error("No device connected")]
    NoDeviceConnected,

    #[error("Device {0} is not connected")]
    DeviceNotConnected(String),

    /// A method without an address was used while several devices are
    /// connected; use the variant taking an address or a [`crate::Connection`].
    #[error("More than one device is connected, the address of the device is required")]
    MultipleDevicesConnected,

    #[error("Device is already connected.")]
    AlreadyConnected,

    #[error("Handler not initialized")]
    HandlerNotInitialized,

    #[error("could not send State: {0}")]
    SendingState(#[from] SendError<bool>),

    #[error("no bluetooth adapters found")]
    NoAdapters,

    #[error("Unknonwn error during disconnect")]
    DisconnectFailed,

    #[error("Unknown error during connect")]
    ConnectionFailed,

    #[error("Mask must match manufacturer data length")]
    InvalidFilterMask,

    #[error("Timeout during execution of {0}")]
    Timeout(String),

    #[error("Failed to join Task: {0}")]
    JoinError(#[from] tokio::task::JoinError),

    /// Something went wrong on the android side of the plugin, or in the JNI
    /// bridge to it.
    #[cfg(target_os = "android")]
    #[error("android: {0}")]
    Android(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.to_string().as_ref())
    }
}

#[cfg(target_os = "android")]
impl From<jni::errors::Error> for Error {
    fn from(e: jni::errors::Error) -> Self {
        Error::Android(e.to_string())
    }
}
