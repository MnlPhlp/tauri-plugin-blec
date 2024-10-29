use std::collections::btree_map::Keys;

use handler::BleHandler;
use once_cell::sync::OnceCell;
use tauri::{
    async_runtime,
    plugin::{Builder, TauriPlugin},
    Manager, Runtime, Wry,
};
use tokio::sync::Mutex;

// #[cfg(target_os = "android")]
mod android;
mod commands;
mod error;
mod handler;
mod models;

static HANDLER: OnceCell<Mutex<BleHandler>> = OnceCell::new();

/// Initializes the plugin.
pub fn init() -> TauriPlugin<Wry> {
    let handler = async_runtime::block_on(BleHandler::new()).expect("failed to initialize handler");
    let _ = HANDLER.set(Mutex::new(handler));

    let builder = Builder::new("blec").invoke_handler(commands::commands());
    #[cfg(target_os = "android")]
    let builder = builder.setup(|app, api| {
        android::init(app, api)?;
        Ok(())
    });
    builder.build()
}

pub fn get_handler() -> error::Result<&'static Mutex<BleHandler>> {
    let handler = HANDLER.get().ok_or(error::Error::HandlerNotInitialized)?;
    Ok(handler)
}