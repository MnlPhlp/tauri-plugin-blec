//! [`blec`](https://crates.io/crates/blec), the cross platform BLE client, for Dioxus.
//!
//! - [`use_ble`] initializes `blec` once for the app and mirrors its state
//!   (ready, scanning, found devices, connected devices) into signals, with
//!   async methods for the usual operations.
//! - [`use_device_notifications`] follows a characteristic of one device for
//!   as long as the component lives, across reconnects.
//!   [`use_ble_notifications`] does the same for the only connected device.
//! - Several devices can be connected at once: [`Ble::connect`] returns a
//!   [`Connection`] to read, write and subscribe on that device.
//! - On android the crate carries the bluetooth permissions and the Kotlin
//!   side of `blec` as a gradle module `dx` builds into the app, so
//!   `Dioxus.toml` only needs `[android] min_sdk = 26`.
//!
//! The full `blec` API stays reachable through [`Ble::handler`] and the
//! re-exported [`blec`] crate.
//!
//! ```no_run
//! use dioxus::prelude::*;
//! use dioxus_blec::{use_ble, models::ScanFilter};
//!
//! #[component]
//! fn Scanner() -> Element {
//!     let ble = use_ble();
//!     rsx! {
//!         button {
//!             disabled: !ble.is_ready() || ble.scanning(),
//!             onclick: move |_| async move {
//!                 if let Err(e) = ble.scan(5000, ScanFilter::None).await {
//!                     tracing::error!("scan failed: {e}");
//!                 }
//!             },
//!             "Scan"
//!         }
//!         ul {
//!             for device in ble.devices() {
//!                 li { key: "{device.address}", "{device.name}" }
//!             }
//!         }
//!     }
//! }
//! ```

pub use blec;
pub use blec::models;
pub use blec::{Connection, ConnectionUpdate, Error, Handler, Result};

use blec::models::{BleDevice, ScanFilter, WriteType};
use blec::OnDisconnectHandler;
use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use uuid::Uuid;

/// Tells `dx` about the `android/` directory next to this crate's `Cargo.toml`,
/// a symlink to `crates/blec/android/lib`.
///
/// `#[manganis::ffi]` embeds the directory's path in the binary the same way
/// assets are embedded. When `dx` builds for android it copies the directory
/// into the generated gradle project as a library module and builds it with
/// the app: the android gradle plugin merges the module's `AndroidManifest.xml`
/// (the bluetooth permissions) into the app's, and the module's Kotlin, the
/// android backend of `blec`, ends up in the apk where `blec` finds it at
/// startup. The class declared here only gives the module its name; `blec`
/// reaches the Kotlin through its own JNI bridge, never through this type.
#[cfg(target_os = "android")]
#[allow(dead_code)]
mod android {
    #[manganis::ffi("android")]
    extern "Kotlin" {
        pub type Blec;
    }
}

/// How far [`use_ble`] got with `blec::init()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitState {
    /// `blec::init()` is still running.
    Pending,
    /// The handler is up; the [`Ble`] methods can be used.
    Ready,
    /// `blec::init()` failed with this message. On android this is where a
    /// missing kotlin side shows up, i.e. an app that did not build the
    /// `android/` module of this crate.
    Failed(String),
}

/// The BLE stack, as signals plus the operations on it.
///
/// `Copy`, like a signal: move it into event handlers freely. Reading one of
/// the state getters inside a component subscribes the component to that
/// piece of state.
///
/// The async methods spawn small forwarding tasks on the Dioxus runtime and
/// therefore have to be called from where Dioxus tasks can be spawned, i.e.
/// from components and event handlers, not from a plain tokio task.
#[derive(Clone, Copy)]
pub struct Ble {
    init: Signal<InitState, SyncStorage>,
    scanning: Signal<bool, SyncStorage>,
    devices: Signal<Vec<BleDevice>, SyncStorage>,
    /// Sorted by address
    connected: Signal<Vec<BleDevice>, SyncStorage>,
}

/// Initializes `blec` once for the whole app and returns its state.
///
/// The first call, from any component, starts `blec::init()` in the
/// background; every call returns the same [`Ble`]. Check [`Ble::init_state`]
/// or [`Ble::is_ready`] before using the handler.
pub fn use_ble() -> Ble {
    use_root_context(|| {
        let ble = Ble {
            init: Signal::new_maybe_sync_in_scope(InitState::Pending, ScopeId::ROOT),
            scanning: Signal::new_maybe_sync_in_scope(false, ScopeId::ROOT),
            devices: Signal::new_maybe_sync_in_scope(Vec::new(), ScopeId::ROOT),
            connected: Signal::new_maybe_sync_in_scope(Vec::new(), ScopeId::ROOT),
        };
        spawn_forever(ble.start());
        ble
    })
}

impl Ble {
    async fn start(mut self) {
        let handler = match blec::init().await {
            Ok(handler) => handler,
            Err(e) => {
                tracing::error!("blec::init failed: {e}");
                self.init.set(InitState::Failed(e.to_string()));
                return;
            }
        };

        let (scan_tx, mut scan_rx) = mpsc::channel(8);
        handler.set_scanning_update_channel(scan_tx).await;
        let mut scanning = self.scanning;
        spawn_forever(async move {
            while let Some(state) = scan_rx.recv().await {
                scanning.set(state);
            }
        });

        let (conn_tx, mut conn_rx) = mpsc::channel(8);
        handler.set_connection_event_channel(conn_tx).await;
        spawn_forever(async move {
            while let Some(update) = conn_rx.recv().await {
                if update.connected {
                    // `connect` fills this in itself once the link is up;
                    // only replace it if the device is known already.
                    if let Ok(device) = handler.connected_device_at(&update.address).await {
                        self.upsert_connected(device);
                    }
                } else {
                    self.remove_connected(&update.address);
                }
            }
        });

        self.init.set(InitState::Ready);
    }

    /// Whether `blec::init()` is pending, done or failed.
    pub fn init_state(&self) -> InitState {
        self.init.cloned()
    }

    /// True once the handler is initialized.
    pub fn is_ready(&self) -> bool {
        *self.init.read() == InitState::Ready
    }

    /// True while a scan is running.
    pub fn scanning(&self) -> bool {
        *self.scanning.read()
    }

    /// The devices found by the current or last [`scan`](Self::scan).
    pub fn devices(&self) -> Vec<BleDevice> {
        self.devices.cloned()
    }

    /// The connected devices, sorted by address. A device is removed on
    /// disconnect, whoever caused it.
    pub fn connected_devices(&self) -> Vec<BleDevice> {
        self.connected.cloned()
    }

    /// The connected device if exactly one is connected, `None` without a
    /// device or with several of them.
    pub fn connected(&self) -> Option<BleDevice> {
        match &self.connected.read()[..] {
            [device] => Some(device.clone()),
            _ => None,
        }
    }

    /// True while at least one device is connected.
    pub fn is_connected(&self) -> bool {
        !self.connected.read().is_empty()
    }

    /// True while the device with this address is connected.
    pub fn is_device_connected(&self, address: &str) -> bool {
        self.connected
            .read()
            .iter()
            .any(|device| device.address == address)
    }

    fn upsert_connected(mut self, device: BleDevice) {
        let mut connected = self.connected.write();
        match connected.binary_search(&device) {
            Ok(i) => connected[i] = device,
            Err(i) => connected.insert(i, device),
        }
    }

    fn remove_connected(mut self, address: &str) {
        self.connected
            .write()
            .retain(|device| device.address != address);
    }

    /// Drops the devices the handler has no connection to anymore.
    fn sync_connected(mut self, handler: &Handler) {
        self.connected
            .write()
            .retain(|device| handler.is_device_connected(&device.address));
    }

    /// The underlying `blec` handler, for everything this type has no method for.
    ///
    /// # Errors
    /// Returns [`Error::HandlerNotInitialized`] until [`is_ready`](Self::is_ready).
    pub fn handler(&self) -> Result<&'static Handler> {
        blec::get_handler()
    }

    /// Checks the bluetooth permissions, asking the user for them if needed.
    ///
    /// Android only; always true elsewhere. With `ask_if_denied`, a user who
    /// denied them before is sent to the app's settings page. A fresh install
    /// gets the system dialog on its first [`scan`](Self::scan) anyway.
    ///
    /// # Errors
    /// Returns an error if the permission check itself fails.
    pub async fn check_permissions(self, ask_if_denied: bool) -> Result<bool> {
        blec::check_permissions(ask_if_denied).await
    }

    /// Scans for `timeout_ms`, streaming the results into [`devices`](Self::devices).
    ///
    /// Clears the previous results first. Returns once the scan has started;
    /// [`scanning`](Self::scanning) turns false when it is over.
    ///
    /// # Errors
    /// Returns an error if the handler is not initialized or the scan cannot start.
    pub async fn scan(mut self, timeout_ms: u64, filter: ScanFilter) -> Result<()> {
        let handler = blec::get_handler()?;
        self.devices.set(Vec::new());
        let (tx, mut rx) = mpsc::channel(8);
        let mut devices = self.devices;
        spawn_forever(async move {
            while let Some(found) = rx.recv().await {
                devices.set(found);
            }
        });
        handler
            .discover(Some(tx), timeout_ms, filter, allow_ibeacons())
            .await
    }

    /// Stops a running scan.
    ///
    /// # Errors
    /// Returns an error if the handler is not initialized or the scan cannot be stopped.
    pub async fn stop_scan(self) -> Result<()> {
        blec::get_handler()?.stop_scan().await
    }

    /// Connects to the device with this address and records it in
    /// [`connected_devices`](Self::connected_devices). Devices that are
    /// already connected stay connected.
    ///
    /// The returned [`Connection`] reads, writes and subscribes on this device.
    ///
    /// # Errors
    /// Returns an error if the handler is not initialized, the device is
    /// unknown or the connection fails.
    pub async fn connect(self, address: &str) -> Result<Connection> {
        let handler = blec::get_handler()?;
        let ble = self;
        let disconnected = address.to_string();
        let on_disconnect = OnDisconnectHandler::from_sync(move || {
            ble.remove_connected(&disconnected);
        });
        let connection = handler
            .connect(address, on_disconnect, allow_ibeacons())
            .await?;
        if let Ok(device) = connection.device().await {
            self.upsert_connected(device);
        }
        Ok(connection)
    }

    /// A [`Connection`] to a device that is connected already.
    ///
    /// # Errors
    /// Returns an error if the handler is not initialized or the device is not
    /// connected.
    pub fn connection(&self, address: &str) -> Result<Connection> {
        blec::get_handler()?.connection(address)
    }

    /// Disconnects from the connected device.
    ///
    /// # Errors
    /// Returns an error if the handler is not initialized, no device or more
    /// than one device is connected, or the disconnect fails.
    pub async fn disconnect(self) -> Result<()> {
        let handler = blec::get_handler()?;
        let result = handler.disconnect().await;
        self.sync_connected(handler);
        result
    }

    /// Disconnects from the device with this address.
    ///
    /// # Errors
    /// Returns an error if the handler is not initialized, the device is not
    /// connected or the disconnect fails.
    pub async fn disconnect_device(self, address: &str) -> Result<()> {
        let handler = blec::get_handler()?;
        let result = handler.disconnect_device(address).await;
        self.sync_connected(handler);
        result
    }

    /// Disconnects from every connected device.
    ///
    /// # Errors
    /// Returns an error if the handler is not initialized or a disconnect fails.
    pub async fn disconnect_all(self) -> Result<()> {
        let handler = blec::get_handler()?;
        let result = handler.disconnect_all().await;
        self.sync_connected(handler);
        result
    }

    /// Writes `data` to a characteristic of the connected device.
    ///
    /// `service` disambiguates characteristics with the same uuid in several services.
    /// With several devices connected, write through their [`Connection`]s instead.
    ///
    /// # Errors
    /// Returns an error if the handler is not initialized, nothing or more
    /// than one device is connected or the write fails.
    pub async fn write(
        self,
        characteristic: Uuid,
        service: Option<Uuid>,
        data: &[u8],
        write_type: WriteType,
    ) -> Result<()> {
        blec::get_handler()?
            .send_data(characteristic, service, data, write_type)
            .await
    }

    /// Reads a characteristic of the connected device.
    ///
    /// With several devices connected, read through their [`Connection`]s instead.
    ///
    /// # Errors
    /// Returns an error if the handler is not initialized, nothing or more
    /// than one device is connected or the read fails.
    pub async fn read(self, characteristic: Uuid, service: Option<Uuid>) -> Result<Vec<u8>> {
        blec::get_handler()?
            .recv_data(characteristic, service)
            .await
    }
}

fn allow_ibeacons() -> bool {
    blec::ALLOW_IBEACONS.load(Ordering::Relaxed)
}

/// The latest notification of a characteristic of the connected device,
/// `None` until the first one.
///
/// Like [`use_device_notifications`], for the device that is connected while
/// it is the only one. With several devices connected it does not subscribe.
pub fn use_ble_notifications(
    characteristic: Uuid,
    service: Option<Uuid>,
) -> ReadSignal<Option<Vec<u8>>, SyncStorage> {
    let ble = use_ble();
    // Only the address matters here: a new `BleDevice` value for the same
    // link (rssi and the like) must not resubscribe.
    let address = use_memo(move || match &ble.connected.read()[..] {
        [device] => Some(device.address.clone()),
        _ => None,
    });
    use_notifications(address, characteristic, service)
}

/// The latest notification of a characteristic of the device with this
/// address, `None` until the first one.
///
/// Subscribes while the device is connected, again after every reconnect (a
/// subscription belongs to one GATT link), and unsubscribes when the component
/// is dropped or `address` changes. Notifications arrive on `blec`'s tasks, so
/// the signal uses the thread safe storage.
pub fn use_device_notifications(
    address: &str,
    characteristic: Uuid,
    service: Option<Uuid>,
) -> ReadSignal<Option<Vec<u8>>, SyncStorage> {
    let ble = use_ble();
    let address = address.to_string();
    // the address while the device is connected
    let linked = use_memo(use_reactive((&address,), move |(address,)| {
        ble.connected
            .read()
            .iter()
            .any(|device| device.address == address)
            .then_some(address)
    }));
    use_notifications(linked, characteristic, service)
}

/// Subscribes to the characteristic on the device `address` names, whenever
/// that changes.
fn use_notifications(
    address: Memo<Option<String>>,
    characteristic: Uuid,
    service: Option<Uuid>,
) -> ReadSignal<Option<Vec<u8>>, SyncStorage> {
    let value = use_signal_sync(|| None::<Vec<u8>>);
    // The device the subscription is on, for unsubscribing. A plain value
    // rather than a signal: it is read while the component is torn down.
    let subscribed = use_hook(|| Arc::new(Mutex::new(None::<String>)));

    let current = subscribed.clone();
    use_resource(move || {
        let address = address();
        let current = current.clone();
        async move {
            let Ok(handler) = blec::get_handler() else {
                return;
            };
            let previous = current.lock().unwrap().take();
            if let Some(previous) = previous.filter(|p| Some(p) != address.as_ref()) {
                if handler.is_device_connected(&previous) {
                    let _ = handler
                        .unsubscribe_from(&previous, characteristic, service)
                        .await;
                }
            }
            let Some(address) = address else {
                return;
            };
            let subscribed = handler
                .subscribe_to(&address, characteristic, service, move |data: Vec<u8>| {
                    // Called more than once, so take a fresh copy of the
                    // signal each time instead of mutating a captured one.
                    let mut value = value;
                    value.set(Some(data));
                })
                .await;
            match subscribed {
                Ok(()) => *current.lock().unwrap() = Some(address),
                Err(e) => tracing::warn!("could not subscribe to {characteristic}: {e}"),
            }
        }
    });

    use_drop(move || {
        let Some(address) = subscribed.lock().unwrap().take() else {
            return;
        };
        spawn_forever(async move {
            if let Ok(handler) = blec::get_handler() {
                if handler.is_device_connected(&address) {
                    let _ = handler
                        .unsubscribe_from(&address, characteristic, service)
                        .await;
                }
            }
        });
    });

    value.into()
}
