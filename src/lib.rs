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

/// Initializes the plugin.
/// # Panics
/// Panics if the handler cannot be initialized.
pub fn init() -> TauriPlugin<Wry> {
    let handler = async_runtime::block_on(Handler::new()).expect("failed to initialize handler");
    let _ = HANDLER.set(handler);

    #[allow(unused)]
    Builder::new("blec")
        .invoke_handler(commands::commands())
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            android::init(app, api)?;
            async_runtime::spawn(handle_events());
            Ok(())
        })
        .build()
}

/// Returns the BLE handler to use blec from rust.
/// # Errors
/// Returns an error if the handler is not initialized.
pub fn get_handler() -> error::Result<&'static Handler> {
    let handler = HANDLER.get().ok_or(error::Error::HandlerNotInitialized)?;
    Ok(handler)
}

/// Checks if the app has the necessary permissions to use BLE.
/// # Errors
/// Returns an error if calling the android plugin fails.
pub fn check_permissions() -> Result<bool, Error> {
    #[cfg(target_os = "android")]
    return Ok(android::check_permissions()?);
    #[cfg(not(target_os = "android"))]
    return Ok(true);
}

async fn handle_events() {
    let handler = get_handler().expect("failed to get handler");
    let stream = handler
        .get_event_stream()
        .await
        .expect("failed to get event stream");
    stream
        .for_each(|event| async {
            handler
                .handle_event(event)
                .await
                .expect("failed to handle event");
        })
        .await;
}
