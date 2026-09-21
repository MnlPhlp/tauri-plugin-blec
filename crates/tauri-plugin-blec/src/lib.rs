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
/// On Android the handler cannot be initialized this early, so it is set up
/// once the app is `Ready` instead. Commands that arrive before that wait for
/// it rather than failing.
///
/// # Errors
/// Returns an error if the handler cannot be initialized. On Android the
/// initialization has not run yet at this point, so its failure surfaces from
/// the commands instead.
pub fn try_init() -> Result<TauriPlugin<Wry>> {
    #[cfg(not(target_os = "android"))]
    async_runtime::block_on(blec::init())?;

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
/// activity to dispatch to is `RunEvent::Ready`.
///
/// `dispatch` only posts the closure to the android main looper and returns, so
/// this finishes some time after `RunEvent::Ready`, possibly after the first
/// command already arrived. The outcome therefore goes to [`android_init`],
/// which is what [`ready`] waits on.
#[cfg(target_os = "android")]
fn init_android() {
    tauri::wry::prelude::dispatch(|env, activity, _webview| {
        // SAFETY: wry calls this on the android main thread with its own JNIEnv
        // and the activity alive for the call. The activity is the `Context`
        // handed to the kotlin side, which also takes it as the one to request
        // runtime permissions from.
        let bridge =
            unsafe { blec::android::init_with(env.get_raw().cast(), activity.as_raw().cast()) };
        let result = bridge
            .and_then(|()| async_runtime::block_on(blec::init()).map(|_| ()))
            .map_err(|e| e.to_string());
        if let Err(e) = &result {
            tracing::error!("could not initialize blec on android: {e}");
        }
        android_init().send_replace(Some(result));
    });
}

/// The result of [`init_android`], `None` until it ran.
///
/// A [`watch`](tokio::sync::watch) rather than a `OnceCell` because [`ready`]
/// has to be able to wait for the value, not just read it.
#[cfg(target_os = "android")]
fn android_init() -> &'static tokio::sync::watch::Sender<Option<std::result::Result<(), String>>> {
    static INIT: std::sync::OnceLock<
        tokio::sync::watch::Sender<Option<std::result::Result<(), String>>>,
    > = std::sync::OnceLock::new();
    INIT.get_or_init(|| tokio::sync::watch::channel(None).0)
}

/// How long a command waits for [`init_android`].
///
/// Only ever the gap between `RunEvent::Ready` and the main looper running the
/// closure, so this never elapses in a working app. It is the last resort for
/// an app whose main thread never gets there, where waiting forever would hang
/// the front-end instead of showing it an error.
#[cfg(target_os = "android")]
const ANDROID_INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Waits until `blec` is initialized, which off Android it already is.
///
/// # Errors
/// Returns the error the initialization failed with, or
/// [`Error::HandlerNotInitialized`] if it has not finished in time.
pub(crate) async fn ready() -> Result<()> {
    #[cfg(target_os = "android")]
    {
        let mut init = android_init().subscribe();
        // `borrow_and_update`, so `changed()` below reports the value that
        // arrives after this check rather than the one already there.
        let mut state = init.borrow_and_update().clone();
        if state.is_none() {
            if tokio::time::timeout(ANDROID_INIT_TIMEOUT, init.changed())
                .await
                .is_err()
            {
                tracing::error!(
                    "blec was not initialized within {ANDROID_INIT_TIMEOUT:?} of the app \
                     becoming ready"
                );
                return Err(Error::HandlerNotInitialized);
            }
            state = init.borrow_and_update().clone();
        }
        match state {
            Some(Ok(())) => {}
            Some(Err(e)) => return Err(Error::Android(e)),
            None => return Err(Error::HandlerNotInitialized),
        }
    }
    Ok(())
}

/// The BLE handler, waiting for the Android initialization if it is still
/// running.
///
/// # Errors
/// Returns an error if the initialization failed or has not finished in time.
pub(crate) async fn handler() -> Result<&'static Handler> {
    ready().await?;
    get_handler()
}
