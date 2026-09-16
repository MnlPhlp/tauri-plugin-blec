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
/// On Android the handler is initialized once the app is `Ready` instead, see
/// [`init_android`]. Until then the commands answer with
/// [`Error::HandlerNotInitialized`].
///
/// # Errors
/// Returns an error if the handler cannot be initialized.
pub fn try_init() -> Result<TauriPlugin<Wry>> {
    #[cfg(not(target_os = "android"))]
    async_runtime::block_on(blec::init())?;

    #[allow(unused)]
    let plugin = Builder::new("blec")
        .invoke_handler(commands::commands())
        .on_event(|_app, event| {
            #[cfg(target_os = "android")]
            if matches!(event, tauri::RunEvent::Ready) {
                init_android();
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

/// Initializes `blec` on Android, from the main thread with the `Activity`.
///
/// Tauri never sets up `ndk-context`, which `blec::init()` would otherwise
/// take the `JavaVM` and `Context` from: tao runs the app on a thread of its
/// own and discards the JNI arguments it was started with. The only way to a
/// `JNIEnv` is `wry::prelude::dispatch`, and the first moment it has an
/// activity to dispatch to is `RunEvent::Ready`. The closure runs on the
/// android main thread, which also handles the IPC, so blocking on the (quick)
/// handler setup there means no command can observe a half-initialized state.
#[cfg(target_os = "android")]
fn init_android() {
    tauri::wry::prelude::dispatch(|env, activity, _webview| {
        // SAFETY: wry calls this on the android main thread with its own JNIEnv
        // and the activity alive for the call. The activity is the `Context`
        // handed to the kotlin side, which also takes it as the one to request
        // runtime permissions from.
        let bridge =
            unsafe { blec::android::init_with(env.get_raw().cast(), activity.as_raw().cast()) };
        if let Err(e) = bridge {
            tracing::error!("could not initialize the blec android bridge: {e}");
            return;
        }
        if let Err(e) = async_runtime::block_on(blec::init()) {
            tracing::error!("could not initialize blec: {e}");
        }
    });
}
