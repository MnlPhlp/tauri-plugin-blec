//! A BLE-Client plugin for Tauri.
//!
//! This is a thin Tauri layer over the [`blec`] crate: it registers the
//! commands the JavaScript API calls and keeps the handler alive for the
//! lifetime of the app. The Rust API is [`blec`]'s, re-exported here so
//! `tauri_plugin_blec::get_handler()` keeps working.

use tauri::{
    async_runtime,
    plugin::{Builder, TauriPlugin},
    Wry,
};

mod commands;

#[cfg(feature = "mock")]
pub use blec::mock;
pub use blec::models;
pub use blec::{
    check_permissions, get_handler, Error, Handler, OnDisconnectHandler, Result,
    SubscriptionHandler, Timeouts, TimeoutsMs, ALLOW_IBEACONS,
};

/// Builds the plugin, initializing the BLE handler.
///
/// # Errors
/// Returns an error if the handler cannot be initialized.
pub fn try_init() -> Result<TauriPlugin<Wry>> {
    async_runtime::block_on(blec::init())?;

    #[allow(unused)]
    let plugin = Builder::new("blec")
        .invoke_handler(commands::commands())
        .on_event(|_app, event| {
            // The kotlin side needs an Activity to request the runtime
            // permissions from, and only the host knows one. Wait for `Ready`:
            // before that there is no activity for wry to dispatch to.
            #[cfg(target_os = "android")]
            if matches!(event, tauri::RunEvent::Ready) {
                tauri::wry::prelude::dispatch(|env, activity, _webview| {
                    // SAFETY: wry calls this on the android main thread with its
                    // own JNIEnv and the activity alive for the call.
                    if let Err(e) = unsafe {
                        blec::android::set_activity(env.get_raw().cast(), activity.as_raw().cast())
                    } {
                        tracing::error!("could not hand the activity to blec: {e}");
                    }
                });
            }
            // Leaving a GATT link open past the end of the process keeps the
            // peripheral "connected" until it times out on its own, during
            // which it stops advertising and looks like it disappeared.
            if matches!(
                event,
                tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. }
            ) {
                if let Ok(handler) = get_handler() {
                    if handler.is_connected() {
                        let _ = async_runtime::block_on(handler.disconnect());
                    }
                }
            }
        })
        .build();
    Ok(plugin)
}

/// Initializes the plugin.
/// # Panics
/// Panics if the handler cannot be initialized.
#[must_use]
pub fn init() -> TauriPlugin<Wry> {
    try_init().expect("failed to initialize plugin")
}
