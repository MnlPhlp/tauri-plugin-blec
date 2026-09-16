use serde::{Serialize, Serializer};
use tokio::sync::mpsc::error::SendError;

use thiserror::Error;
#[derive(Debug, Error)]
pub enum Error {
    #[error("Btleplug error: {0}")]
    Btleplug(#[from] btleplug::Error),

    #[error("There is no peripheral with id: {0}")]
    UnknownPeripheral(String),

    #[error("Characteristic {0} not available")]
    CharacNotAvailable(String),

    #[error("No device connected")]
    NoDeviceConnected,

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

    #[cfg(target_os = "android")]
    #[error(transparent)]
    PluginInvoke(#[from] tauri::plugin::mobile::PluginInvokeError),
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
