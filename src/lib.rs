use std::sync::LazyLock;

use futures::StreamExt;
use once_cell::sync::OnceCell;
use tauri::{
    async_runtime,
    plugin::{Builder, TauriPlugin},
    Wry,
};

#[cfg(target_os = "android")]
mod android;
mod commands;
mod error;
mod handler;
pub mod models;

pub use error::Error;
pub use handler::Handler;
pub use handler::{OnDisconnectHandler, SubscriptionHandler};
use tokio::sync::Mutex;

pub static ALLOW_IBEACONS: LazyLock<Mutex<bool>> = LazyLock::new(|| Mutex::new(false));

static HANDLER: OnceCell<Handler> = OnceCell::new();

pub fn try_init() -> Result<TauriPlugin<Wry>, Error> {
    let handler = async_runtime::block_on(Handler::new())?;
    let _ = HANDLER.set(handler);

    #[allow(unused)]
    let plugin = Builder::new("blec")
        .invoke_handler(commands::commands())
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            android::init(app, api)?;
            Ok(())
        })
        .build();
    Ok(plugin)
}

/// Initializes the plugin.
/// # Panics
/// Panics if the handler cannot be initialized.
pub fn init() -> TauriPlugin<Wry> {
    try_init().expect("failed to initialize plugin")
}

/// Returns the BLE handler to use blec from rust.
/// # Errors
/// Returns an error if the handler is not initialized.
pub fn get_handler() -> error::Result<&'static Handler> {
    let handler = HANDLER.get().ok_or(error::Error::HandlerNotInitialized)?;
    Ok(handler)
}

/// Checks if the app has the necessary permissions to use BLE.
/// If `ask_if_denied` is true, the user will be prompted again to grant permissions if they
/// previously denied.
/// # Errors
/// Returns an error if calling the android plugin fails.
#[allow(unused)]
pub fn check_permissions(ask_if_denied: bool) -> Result<bool, Error> {
    #[cfg(target_os = "android")]
    return Ok(android::check_permissions(ask_if_denied)?);
    #[cfg(not(target_os = "android"))]
    return Ok(true);
}
