use crate::error::Error;
use crate::models::{self, fmt_addr, AdapterState, BleDevice, ScanFilter, Service, Timeouts};
use crate::ALLOW_IBEACONS;
use btleplug::api::{Central, Characteristic, Manager as _, Peripheral as _};
use btleplug::api::{CentralEvent, CentralState};
use btleplug::platform::PeripheralId;
use futures::{Stream, StreamExt};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::sleep;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

// The BLE backend: the mock wins whenever it is enabled, otherwise the plugin's
// own android implementation or btleplug's platform backend.
#[cfg(all(target_os = "android", not(any(test, feature = "mock"))))]
use crate::android::{Adapter, Manager, Peripheral};
#[cfg(any(test, feature = "mock"))]
use crate::mock::{Adapter, Manager, Peripheral};
#[cfg(not(any(target_os = "android", test, feature = "mock")))]
use btleplug::platform::{Adapter, Manager, Peripheral};

#[cfg(test)]
mod tests;

struct Listener {
    uuid: Uuid,
    service: Uuid,
    callback: SubscriptionHandler,
}

struct HandlerState {
    characs: Vec<Characteristic>,
    listen_handle: Option<tokio::task::JoinHandle<()>>,
    on_disconnect: OnDisconnectHandler,
    connection_update_channel: Vec<mpsc::Sender<bool>>,
    scan_update_channel: Vec<mpsc::Sender<bool>>,
    scan_task: Option<tokio::task::JoinHandle<()>>,
}

impl HandlerState {
    fn get_charac(&self, uuid: Uuid) -> Result<&Characteristic, Error> {
        trace!("getting characteristic {uuid}");
        let charac = self.characs.iter().find(|c| c.uuid == uuid);
        charac.ok_or(Error::CharacNotAvailable(uuid.to_string()))
    }

    fn get_charac_from_service(&self, uuid: Uuid, service: Uuid) -> Result<&Characteristic, Error> {
        trace!("getting characteristic {uuid} from service {service}");
        let charac = self
            .characs
            .iter()
            .find(|c| c.uuid == uuid && c.service_uuid == service);
        charac.ok_or(Error::CharacNotAvailable(uuid.to_string()))
    }
}

pub struct Handler {
    devices: Arc<Mutex<HashMap<String, Peripheral>>>,
    adapter: Mutex<Option<Arc<Adapter>>>,
    /// Task forwarding the adapter's `CentralEvent`s to `handle_event`
    event_pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
    notify_listeners: Arc<Mutex<Vec<Listener>>>,
    connected_rx: watch::Receiver<bool>,
    connected_tx: watch::Sender<bool>,
    state: Mutex<HandlerState>,
    connected_dev: Mutex<Option<Peripheral>>,
    /// Lock to serialize BLE GATT operations (only one read/write can be in flight at a time)
    gatt_op_lock: Mutex<()>,
    timeouts: std::sync::Mutex<Timeouts>,

    write_timeout_in_ms: AtomicU32,
    skip_waiting_for_write_to_complete: AtomicBool,
}

impl Handler {
    /// Replaces the timeouts used for the GATT operations.
    /// See [`Timeouts`] for the defaults.
    pub fn set_timeouts(&self, timeouts: Timeouts) {
        info!("setting timeouts to {timeouts:?}");
        *self
            .timeouts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = timeouts;
    }

    /// The timeouts currently used for the GATT operations.
    #[must_use]
    pub fn timeouts(&self) -> Timeouts {
        *self
            .timeouts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn set_write_behaviour(&self, timeout_in_ms: Option<u32>, skip_waiting_on_success: bool) {
        self.write_timeout_in_ms.store(
            timeout_in_ms.unwrap_or(0),
            std::sync::atomic::Ordering::Release,
        );
        self.skip_waiting_for_write_to_complete.store(
            skip_waiting_on_success,
            std::sync::atomic::Ordering::Release,
        );
    }

    #[allow(dead_code)] // TODO: remove once implemented on all platforms
    pub(crate) fn get_write_behaviour(&self) -> (u32, bool) {
        let timeout = self
            .write_timeout_in_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let skip_waiting_on_success = self
            .skip_waiting_for_write_to_complete
            .load(std::sync::atomic::Ordering::Relaxed);
        (timeout, skip_waiting_on_success)
    }

    /// Sets the MTU that is requested when connecting on Android.
    /// Other plarforms will always negotiate the max by default
    /// The actual MTU can be retrieved using the `mtu` method after connecting
    /// 0 means no mtu request will be made
    #[cfg(target_os = "android")]
    pub fn set_android_mtu_request(mtu: u16) {
        crate::android::REQUESTED_MTU.store(mtu, std::sync::atomic::Ordering::Release);
    }
}

async fn get_central() -> Result<Adapter, Error> {
    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let central = adapters.into_iter().next().ok_or(Error::NoAdapters)?;
    Ok(central)
}

pub enum OnDisconnectHandler {
    None,
    Sync(Box<dyn FnOnce() + Send>),
    Async(Box<dyn FnOnce() -> Box<dyn Future<Output = ()> + Send + Unpin> + Send>),
}
impl OnDisconnectHandler {
    async fn run(self) {
        match self {
            OnDisconnectHandler::None => {}
            OnDisconnectHandler::Sync(f) => f(),
            OnDisconnectHandler::Async(f) => f().await,
        }
    }

    #[must_use]
    pub fn take(&mut self) -> Self {
        std::mem::replace(self, OnDisconnectHandler::None)
    }

    pub fn from_async<F, FUTURE>(func: F) -> Self
    where
        F: FnOnce() -> FUTURE + Send + 'static,
        FUTURE: Future<Output = ()> + Send + 'static,
    {
        OnDisconnectHandler::Async(Box::new(move || Box::new(Box::pin(func()))))
    }

    pub fn from_sync<F: FnOnce() + Send + 'static>(func: F) -> Self {
        OnDisconnectHandler::Sync(Box::new(func))
    }
}

#[allow(clippy::type_complexity)]
pub enum SubscriptionHandler {
    Sync(Box<dyn Fn(Vec<u8>) + Send + Sync>),
    Async(Box<dyn Fn(Vec<u8>) -> Box<dyn Future<Output = ()> + Send + Unpin> + Send + Sync>),
}

impl SubscriptionHandler {
    pub fn from_async<F, FUTURE>(func: F) -> Self
    where
        F: Fn(Vec<u8>) -> FUTURE + Send + Sync + 'static,
        FUTURE: Future<Output = ()> + Send + 'static,
    {
        SubscriptionHandler::Async(Box::new(move |data| Box::new(Box::pin(func(data)))))
    }

    async fn run(&self, data: Vec<u8>) {
        match self {
            SubscriptionHandler::Sync(f) => f(data),
            SubscriptionHandler::Async(f) => f(data).await,
        }
    }
}

impl<F: Fn(Vec<u8>) + Send + Sync + 'static> From<F> for SubscriptionHandler {
    fn from(func: F) -> Self {
        SubscriptionHandler::Sync(Box::new(func))
    }
}

impl Handler {
    pub(crate) async fn new() -> Result<Self, Error> {
        Ok(Self::with_adapter(None))
    }

    /// Creates a handler that uses `adapter` instead of the first adapter the
    /// platform manager reports. Used to inject a [`crate::mock`] adapter.
    #[cfg(any(test, feature = "mock"))]
    #[allow(dead_code)] // only the tests inject an adapter so far
    pub(crate) fn new_with_adapter(adapter: Adapter) -> Self {
        Self::with_adapter(Some(Arc::new(adapter)))
    }

    fn with_adapter(adapter: Option<Arc<Adapter>>) -> Self {
        let (connected_tx, connected_rx) = watch::channel(false);
        Self {
            devices: Arc::new(Mutex::new(HashMap::new())),
            adapter: Mutex::new(adapter),
            event_pump: Mutex::new(None),
            notify_listeners: Arc::new(Mutex::new(vec![])),
            connected_rx,
            connected_tx,
            connected_dev: Mutex::new(None),
            gatt_op_lock: Mutex::new(()),
            timeouts: std::sync::Mutex::new(Timeouts::default()),
            state: Mutex::new(HandlerState {
                on_disconnect: OnDisconnectHandler::None,
                connection_update_channel: vec![],
                scan_task: None,
                scan_update_channel: vec![],
                listen_handle: None,
                characs: vec![],
            }),
            write_timeout_in_ms: AtomicU32::new(0),
            skip_waiting_for_write_to_complete: AtomicBool::new(false),
        }
    }

    /// Returns the adapter, creating it on first use, and makes sure the task
    /// that forwards the adapter's events to [`Self::handle_event`] is running.
    async fn get_or_init_adapter(&'static self) -> Result<Arc<Adapter>, Error> {
        let adapter = {
            let mut adapter_guard = self.adapter.lock().await;
            match &*adapter_guard {
                Some(adapter) => adapter.clone(),
                None => {
                    let adapter = Arc::new(get_central().await?);
                    *adapter_guard = Some(adapter.clone());
                    adapter
                }
            }
        };
        self.start_event_pump(&adapter).await;
        Ok(adapter)
    }

    async fn start_event_pump(&'static self, adapter: &Arc<Adapter>) {
        let mut pump = self.event_pump.lock().await;
        if pump.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        let stream_res = self.get_event_stream_internal(adapter.clone()).await;
        *pump = Some(tokio::task::spawn(async move {
            match stream_res {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        let _ = self.handle_event(event).await;
                    }
                    warn!("adapter event stream ended");
                }
                Err(e) => error!("failed to get adapter event stream: {e}"),
            }
        }));
    }

    async fn get_event_stream_internal(
        &self,
        adapter: Arc<Adapter>,
    ) -> Result<Pin<Box<dyn Stream<Item = CentralEvent> + Send>>, Error> {
        let events = adapter.events().await?;
        Ok(events)
    }

    /// Returns true if a device is connected
    pub fn is_connected(&self) -> bool {
        *self.connected_rx.borrow()
    }

    /// Returns mtu (Maximum Transfer Unit) of the currently connected device
    /// # Errors
    /// Returns an error if no device is connected
    pub async fn mtu(&self) -> Result<u16, Error> {
        let connected_dev = self.connected_dev.lock().await;
        let Some(dev) = connected_dev.as_ref() else {
            return Err(Error::NoDeviceConnected);
        };
        Ok(dev.mtu())
    }

    /// Returns true if the adapter is scanning
    pub async fn is_scanning(&self) -> bool {
        if let Some(handle) = &self.state.lock().await.scan_task {
            !handle.is_finished()
        } else {
            false
        }
    }

    /// Takes a sender that will be used to send changes in the scanning status
    /// # Example
    /// ```no_run
    /// use tauri::async_runtime;
    /// use tokio::sync::mpsc;
    /// async_runtime::block_on(async {
    ///     let handler = tauri_plugin_blec::get_handler().unwrap();
    ///     let (tx, mut rx) = mpsc::channel(1);
    ///     handler.set_scanning_update_channel(tx).await;
    ///     while let Some(scanning) = rx.recv().await {
    ///         println!("Scanning: {scanning}");
    ///     }
    /// });
    /// ```
    pub async fn set_scanning_update_channel(&self, tx: mpsc::Sender<bool>) {
        self.state.lock().await.scan_update_channel.push(tx);
    }

    /// Takes a sender that will be used to send changes in the connection status
    /// # Example
    /// ```no_run
    /// use tauri::async_runtime;
    /// use tokio::sync::mpsc;
    /// async_runtime::block_on(async {
    ///     let handler = tauri_plugin_blec::get_handler().unwrap();
    ///     let (tx, mut rx) = mpsc::channel(1);
    ///     handler.set_connection_update_channel(tx).await;
    ///     while let Some(connected) = rx.recv().await {
    ///         println!("Connected: {connected}");
    ///     }
    /// });
    /// ```
    pub async fn set_connection_update_channel(&self, tx: mpsc::Sender<bool>) {
        self.state.lock().await.connection_update_channel.push(tx);
    }

    /// Connects to the given address
    /// If a callback is provided, it will be called when the device is disconnected.
    /// Because connecting sometimes fails especially on android, this method tries up to 3 times
    /// before returning an error
    /// # Errors
    /// Returns an error if no devices are found, if the device is already connected,
    /// if the connection fails, or if the service/characteristics discovery fails
    /// # Example
    /// ```no_run
    /// use tauri::async_runtime;
    /// use tauri_plugin_blec::OnDisconnectHandler;
    /// async_runtime::block_on(async {
    ///    let handler = tauri_plugin_blec::get_handler().unwrap();
    ///    handler.connect("00:00:00:00:00:00", OnDisconnectHandler::from_sync(|| println!("disconnected")), false).await.unwrap();
    /// });
    /// ```
    pub async fn connect(
        &'static self,
        address: &str,
        on_disconnect: OnDisconnectHandler,
        allow_ibeacons: bool,
    ) -> Result<(), Error> {
        if !self.devices.lock().await.contains_key(address) {
            // run a short scan to try and find the device
            let (tx, mut rx) = mpsc::channel(8);
            self.discover(Some(tx), 1000, ScanFilter::None, allow_ibeacons)
                .await?;
            while let Some(devices) = rx.recv().await {
                for dev in devices {
                    if dev.address == address {
                        break;
                    }
                }
            }
        }
        // cancel any running discovery
        let _ = self.stop_scan().await;
        // Never leave a previous link open: connecting to a second device while
        // the first one is still attached leaks an OS level GATT connection.
        self.disconnect_other(address).await;
        // connect to the given address
        // try up to 3 times before returning an error
        let mut connected = Ok(());
        for i in 0..3 {
            if let Err(e) = self.connect_device(address).await {
                if i < 2 {
                    warn!("Failed to connect device, retrying in 1s: {e}");
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
                connected = Err(e);
            } else {
                connected = Ok(());
                break;
            }
        }
        if let Err(e) = connected {
            error!("Failed to connect device: {e}");
            // A `connectGatt` may still be pending; cancel it so the device is
            // not left half-connected.
            let device = self.connected_dev.lock().await.clone();
            if let Some(device) = device {
                if let Err(e) = device.disconnect().await {
                    warn!("Failed to cancel the pending connection: {e}");
                }
            }
            self.clear_connection_state().await;
            return Err(e);
        }
        debug!("connecting services");
        // discover service/characteristics (no state lock held during GATT op)
        let characs = match self.connect_services().await {
            Ok(characs) => characs,
            Err(e) => {
                // The link is up but unusable — tear it down instead of
                // returning with a connected device the caller doesn't know about.
                error!("Failed to discover services: {e}");
                if let Err(e) = self.disconnect().await {
                    warn!("Failed to disconnect after failed service discovery: {e}");
                }
                return Err(e);
            }
        };
        {
            debug!("locking state");
            let mut state = self.state.lock().await;
            // set callback to run on disconnect
            state.on_disconnect = on_disconnect;
            state.characs = characs;
            debug!("Starting notification task");
            // start background task for notifications
            state.listen_handle = Some(tokio::task::spawn(listen_notify(
                self.connected_dev.lock().await.clone(),
                self.notify_listeners.clone(),
            )));
        }
        self.send_connection_update(true).await;
        info!("connecting done");
        Ok(())
    }

    async fn connect_services(&self) -> Result<Vec<Characteristic>, Error> {
        let device = {
            let dev = self.connected_dev.lock().await;
            dev.as_ref().ok_or(Error::NoDeviceConnected)?.clone()
        };
        debug!("starting service discovery");
        {
            let _gatt_guard = self.gatt_op_lock.lock().await;
            run_with_timeout(
                device.discover_services(),
                "discover services",
                self.timeouts().discover_services,
            )
            .await?;
        }
        debug!("service discovery done");
        let mut characs = vec![];
        for s in device.services() {
            for c in &s.characteristics {
                characs.push(c.clone());
            }
        }
        Ok(characs)
    }

    async fn connect_device(&self, address: &str) -> Result<(), Error> {
        trace!("connect_device: initiating connection to {address}");
        debug!("connecting to {address}",);
        let mut connected_rx = self.connected_rx.clone();
        // clone the peripheral and release the lock: `handle_connect` needs it
        // while we are waiting for the connection event
        let device = {
            let devices = self.devices.lock().await;
            devices
                .get(address)
                .ok_or(Error::UnknownPeripheral(address.to_string()))?
                .clone()
        };
        {
            *self.connected_dev.lock().await = Some(device.clone());
            if device.is_connected().await? {
                debug!("Device already connected");
                self.connected_tx
                    .send(true)
                    .expect("failed to send connected update");
                return Ok(());
            }
        }
        debug!("Connecting to device");
        let timeout = self.timeouts().connect;
        {
            let _gatt_guard = self.gatt_op_lock.lock().await;
            run_with_timeout(device.connect(), "Connect", timeout).await?;
        }
        // wait for the actual connection to be established
        if !*connected_rx.borrow_and_update() {
            info!("waiting for connection event");
            match tokio::time::timeout(timeout, connected_rx.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    error!("connection event channel closed: {e}");
                    return Err(Error::ConnectionFailed);
                }
                Err(_) => {
                    warn!("timed out waiting for the connection event");
                    // the connection may still be established later; make sure
                    // it isn't left dangling
                    if let Err(e) = device.disconnect().await {
                        warn!("Failed to cancel the pending connection: {e}");
                    }
                    return Err(Error::Timeout("connect event".to_string()));
                }
            }
        }
        if !*self.connected_rx.borrow() {
            // still not connected
            warn!("Still not connected after connection event");
            return Err(Error::ConnectionFailed);
        }
        trace!("connect_device: connection established to {address}");
        info!("device connected");
        Ok(())
    }

    /// Disconnects a currently connected device unless it is the one at
    /// `address` and still actually connected. Best effort: errors are logged.
    async fn disconnect_other(&self, address: &str) {
        let device = self.connected_dev.lock().await.clone();
        let Some(device) = device else {
            return;
        };
        if fmt_addr(device.address()) == address && device.is_connected().await.unwrap_or(false) {
            // `connect_device` reuses this connection
            return;
        }
        info!(
            "disconnecting {} before connecting to {address}",
            fmt_addr(device.address())
        );
        if let Err(e) = self.disconnect().await {
            warn!("Failed to disconnect the previously connected device: {e}");
        }
    }

    /// Drops every trace of a connection: forgets the device, stops the
    /// notification task, clears the characteristics and listeners, runs the
    /// disconnect callback and reports the new state to all listeners.
    async fn clear_connection_state(&self) {
        *self.connected_dev.lock().await = None;
        *self.notify_listeners.lock().await = vec![];
        {
            // scope: `send_connection_update` needs the state lock as well
            let mut state = self.state.lock().await;
            if let Some(handle) = state.listen_handle.take() {
                handle.abort();
            }
            state.on_disconnect.take().run().await;
            state.characs.clear();
        }
        self.send_connection_update(false).await;
        let _ = self.connected_tx.send(false);
    }

    /// Disconnects from the connected device
    /// This triggers a disconnect and then waits for the actual disconnect event from the adapter.
    ///
    /// The disconnect is always attempted on the peripheral, even when the
    /// internal state says it is no longer connected: on Android that state can
    /// be out of sync with a still open GATT link. A state desync is therefore
    /// repaired instead of reported, and only a missing device is an error.
    /// # Errors
    /// Returns an error if no device is connected
    pub async fn disconnect(&self) -> Result<(), Error> {
        trace!("disconnect: user-initiated disconnect");
        info!("disconnect triggered by user");
        let mut connected_rx = self.connected_rx.clone();
        let was_connected = *connected_rx.borrow_and_update();
        // Clone: the lock must not be held while waiting for the disconnect event
        let dev = self.connected_dev.lock().await.clone();
        let Some(dev) = dev else {
            debug!("no device connected");
            return Err(Error::NoDeviceConnected);
        };
        let is_connected = dev.is_connected().await.unwrap_or(false);
        if let Err(e) = dev.disconnect().await {
            warn!("Failed to trigger disconnect: {e}");
        }
        if !is_connected || !was_connected {
            // No disconnect event will arrive because the adapter doesn't
            // consider the device connected anymore. Repair the state.
            warn!("device was already disconnected, clearing connection state");
            self.clear_connection_state().await;
            return Ok(());
        }
        // the change will be triggered by handle_event -> handle_disconnect which runs in another
        // task
        let timeout = self.timeouts().disconnect;
        match tokio::time::timeout(timeout, connected_rx.changed()).await {
            Ok(Ok(())) if !*self.connected_rx.borrow() => return Ok(()),
            Ok(Ok(())) => warn!("still connected after the disconnect event"),
            Ok(Err(e)) => warn!("disconnect event channel closed: {e}"),
            Err(_) => warn!("timed out waiting for the disconnect event"),
        }
        // The platform side closes the GATT link on its own; make sure our
        // state agrees so the next connect isn't blocked by a stale device.
        self.clear_connection_state().await;
        Ok(())
    }

    /// Clears internal state, updates connected flag and calls disconnect callback
    async fn handle_disconnect(&self, peripheral_id: PeripheralId) -> Result<(), Error> {
        trace!("handle_disconnect: DeviceDisconnected event for {peripheral_id}");
        info!("Handling disconnect for {peripheral_id}");
        let connected = self
            .connected_dev
            .lock()
            .await
            .as_ref()
            .map(btleplug::api::Peripheral::id);
        if !connected.as_ref().is_some_and(|c| *c == peripheral_id) {
            // event not for currently connected device, ignore
            warn!("Unexpected disconnect event for device {peripheral_id}, connected device is {connected:?}",);
            return Ok(());
        }
        info!("disconnecting");
        self.clear_connection_state().await;
        Ok(())
    }

    /// Scans for `timeout` milliseconds and periodically sends discovered devices
    /// to the given channel.
    /// A task is spawned to handle the scan and send the devices, so the function
    /// returns immediately.
    ///
    /// A Variant of [`ScanFilter`] can be provided to filter the discovered devices
    /// When `allow_ibeacons` is set to true, android will request fine location permission to
    /// allow finding and connecting to iBeacons.
    ///
    /// # Errors
    /// Returns an error if starting the scan fails
    /// # Panics
    /// Panics if there is an error getting devices from the adapter
    /// # Example
    /// ```no_run
    /// use tauri::async_runtime;
    /// use tokio::sync::mpsc;
    /// use tauri_plugin_blec::models::ScanFilter;
    ///
    /// async_runtime::block_on(async {
    ///     let handler = tauri_plugin_blec::get_handler().unwrap();
    ///     let (tx, mut rx) = mpsc::channel(1);
    ///     handler.discover(Some(tx),1000, ScanFilter::None, false).await.unwrap();
    ///     while let Some(devices) = rx.recv().await {
    ///         println!("Discovered {devices:?}");
    ///     }
    /// });
    /// ```
    pub async fn discover(
        &'static self,
        tx: Option<mpsc::Sender<Vec<BleDevice>>>,
        timeout: u64,
        filter: ScanFilter,
        allow_ibeacons: bool,
    ) -> Result<(), Error> {
        if let ScanFilter::ManufacturerDataMasked(_, ref data, ref mask) = filter {
            if data.len() != mask.len() {
                return Err(Error::InvalidFilterMask);
            }
        }
        let adapter = self.get_or_init_adapter().await?;
        {
            let mut state = self.state.lock().await;
            // stop any ongoing scan (best-effort — scan may have already
            // been stopped by the polling task)
            if let Some(handle) = state.scan_task.take() {
                handle.abort();
                let _ = adapter.stop_scan().await;
            }
            // start a new scan
            ALLOW_IBEACONS.store(allow_ibeacons, std::sync::atomic::Ordering::Release);
            adapter
                .start_scan(btleplug::api::ScanFilter::default())
                .await?;
        }
        self.send_scan_update(true).await;
        let mut state = self.state.lock().await;
        let mut self_devices = self.devices.clone();
        let adapter = adapter.clone();
        state.scan_task = Some(tokio::task::spawn(async move {
            self_devices.lock().await.clear();
            let loops = std::cmp::max(1, timeout / 200);
            let mut devices;
            for _ in 0..loops {
                sleep(Duration::from_millis(200)).await;
                let mut discovered = adapter
                    .peripherals()
                    .await
                    .expect("failed to get peripherals");
                filter_peripherals(&mut discovered, &filter).await;
                devices = Self::add_devices(&mut self_devices, discovered).await;
                if !devices.is_empty() {
                    if let Some(tx) = &tx {
                        tx.send(devices.clone())
                            .await
                            .expect("failed to send devices");
                    }
                }
            }
            let _ = adapter
                .stop_scan()
                .await
                .map_err(|e| error!("Failed to stop scan: {e}"));
            self.send_scan_update(false).await;
        }));
        Ok(())
    }

    /// Discover provided services and charecteristics
    /// If the device is not connected, a connection is made in order to discover the services and characteristics
    /// After the discovery is done, the device is disconnected
    /// If the devices was already connected, it will stay connected
    /// # Errors
    /// Returns an error if the device is not found, if the connection fails, or if the discovery fails
    /// # Panics
    /// Panics if there is an error with the internal disconnect event
    pub async fn discover_services(&self, address: &str) -> Result<Vec<Service>, Error> {
        let mut already_connected = self
            .connected_dev
            .lock()
            .await
            .as_ref()
            .is_some_and(|dev| address == fmt_addr(dev.address()));
        let device = if already_connected {
            self.connected_dev
                .lock()
                .await
                .as_ref()
                .expect("Connection exists")
                .clone()
        } else {
            let device = self
                .devices
                .lock()
                .await
                .get(address)
                .ok_or(Error::UnknownPeripheral(address.to_string()))?
                .clone();
            if device.is_connected().await? {
                already_connected = true;
            } else if let Err(e) = self.connect_device(address).await {
                *self.connected_dev.lock().await = None;
                let _ = self.connected_tx.send(false);
                error!("Failed to connect for discovery: {e}");
                return Err(e);
            }
            device
        };
        debug!("discovering services on {address}");
        if device.services().is_empty() {
            let _gatt_guard = self.gatt_op_lock.lock().await;
            run_with_timeout(
                device.discover_services(),
                "discover services",
                self.timeouts().discover_services,
            )
            .await?;
        }
        let services = device.services().iter().map(Service::from).collect();
        if !already_connected {
            let mut connected_rx = self.connected_rx.clone();
            if *connected_rx.borrow_and_update() {
                device.disconnect().await?;
                debug!("waiting for disconnect event");
                connected_rx
                    .changed()
                    .await
                    .expect("failed to wait for disconnect event");
            }
        }
        Ok(services)
    }

    /// Stops scanning for devices
    /// # Errors
    /// Stops an ongoing scan. The polling task is aborted first, then the
    /// adapter scan is stopped (best-effort — it may have already been
    /// stopped by the polling task finishing).
    pub async fn stop_scan(&'static self) -> Result<(), Error> {
        if let Some(handle) = self.state.lock().await.scan_task.take() {
            handle.abort();
        }
        let adapter = self.get_or_init_adapter().await?;
        let _ = adapter.stop_scan().await;
        self.send_scan_update(false).await;
        Ok(())
    }

    async fn add_devices(
        self_devices: &mut Arc<Mutex<HashMap<String, Peripheral>>>,
        discovered: Vec<Peripheral>,
    ) -> Vec<BleDevice> {
        let mut devices = vec![];
        for p in discovered {
            match BleDevice::from_peripheral(&p).await {
                Ok(dev) => {
                    self_devices.lock().await.insert(dev.address.clone(), p);
                    devices.push(dev);
                }
                Err(e) => {
                    warn!("Failed to add device: {e}");
                }
            }
        }
        devices.sort();
        devices
    }

    /// Sends data to the given characteristic of the connected device
    /// # Errors
    /// Returns an error if no device is connected or the characteristic is not available
    /// or if the write operation fails
    /// # Example
    /// ```no_run
    /// use tauri::async_runtime;
    /// use uuid::{Uuid,uuid};
    /// use tauri_plugin_blec::models::WriteType;
    ///
    /// const CHARACTERISTIC_UUID: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");
    /// async_runtime::block_on(async {
    ///     let handler = tauri_plugin_blec::get_handler().unwrap();
    ///     let data = [1,2,3,4,5];
    ///     let response = handler.send_data(CHARACTERISTIC_UUID, None, &data, WriteType::WithResponse).await.unwrap();
    /// });
    /// ```
    pub async fn send_data(
        &self,
        c: Uuid,
        service: Option<Uuid>,
        data: &[u8],
        write_type: models::WriteType,
    ) -> Result<(), Error> {
        let _gatt_guard = self.gatt_op_lock.lock().await;
        let dev = {
            let dev_guard = self.connected_dev.lock().await;
            dev_guard
                .as_ref()
                .cloned()
                .ok_or(Error::NoDeviceConnected)?
        };
        let charac = {
            let state = self.state.lock().await;
            if let Some(service) = service {
                state.get_charac_from_service(c, service)?.clone()
            } else {
                state.get_charac(c)?.clone()
            }
        };

        trace!(
            "sending {} bytes to characteristic {c}: {:02x?}",
            data.len(),
            data
        );
        run_with_timeout(
            dev.write(&charac, data, write_type.into()),
            "write",
            self.timeouts().write,
        )
        .await?;
        Ok(())
    }

    /// Receives data from the given characteristic of the connected device
    /// Returns the data as a vector of bytes
    /// # Errors
    /// Returns an error if no device is connected or the characteristic is not available
    /// or if the read operation fails
    /// # Example
    /// ```no_run
    /// use tauri::async_runtime;
    /// use uuid::{Uuid,uuid};
    /// const CHARACTERISTIC_UUID: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");
    /// async_runtime::block_on(async {
    ///     let handler = tauri_plugin_blec::get_handler().unwrap();
    ///     let response = handler.recv_data(CHARACTERISTIC_UUID, None).await.unwrap();
    /// });
    /// ```
    pub async fn recv_data(&self, c: Uuid, service: Option<Uuid>) -> Result<Vec<u8>, Error> {
        let _gatt_guard = self.gatt_op_lock.lock().await;
        let dev = {
            let dev_guard = self.connected_dev.lock().await;
            dev_guard
                .as_ref()
                .cloned()
                .ok_or(Error::NoDeviceConnected)?
        };
        let charac = {
            let state = self.state.lock().await;
            if let Some(service) = service {
                state.get_charac_from_service(c, service)?.clone()
            } else {
                state.get_charac(c)?.clone()
            }
        };
        let data = run_with_timeout(dev.read(&charac), "read", self.timeouts().read).await?;
        trace!(
            "received {} bytes from characteristic {c}: {:02x?}",
            data.len(),
            data
        );
        Ok(data)
    }

    /// Subscribe to notifications from the given characteristic
    /// The callback will be called whenever a notification is received
    /// # Errors
    /// Returns an error if no device is connected or the characteristic is not available
    /// or if the subscribe operation fails
    /// # Example
    /// ```no_run
    /// use tauri::async_runtime;
    /// use uuid::{Uuid,uuid};
    /// const CHARACTERISTIC_UUID: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");
    /// async_runtime::block_on(async {
    ///     let handler = tauri_plugin_blec::get_handler().unwrap();
    ///     let response = handler.subscribe(CHARACTERISTIC_UUID, None, |data| println!("received {data:?}")).await.unwrap();
    /// });
    /// ```
    pub async fn subscribe(
        &self,
        c: Uuid,
        service: Option<Uuid>,
        callback: impl Into<SubscriptionHandler>,
    ) -> Result<(), Error> {
        let _gatt_guard = self.gatt_op_lock.lock().await;
        let dev = {
            let dev_guard = self.connected_dev.lock().await;
            dev_guard
                .as_ref()
                .cloned()
                .ok_or(Error::NoDeviceConnected)?
        };
        let charac = {
            let state = self.state.lock().await;
            if let Some(service) = service {
                state.get_charac_from_service(c, service)?.clone()
            } else {
                state.get_charac(c)?.clone()
            }
        };
        info!("subscribing to characteristic {charac:?}");
        run_with_timeout(
            dev.subscribe(&charac),
            "subscribe",
            self.timeouts().subscribe,
        )
        .await?;
        info!("subscribed successfully");
        self.notify_listeners.lock().await.push(Listener {
            uuid: charac.uuid,
            service: charac.service_uuid,
            callback: callback.into(),
        });
        Ok(())
    }

    /// Unsubscribe from notifications for the given characteristic
    /// This will also remove the callback from the list of listeners
    /// # Errors
    /// Returns an error if no device is connected or the characteristic is not available
    /// or if the unsubscribe operation fails
    pub async fn unsubscribe(&self, c: Uuid) -> Result<(), Error> {
        let _gatt_guard = self.gatt_op_lock.lock().await;
        let dev = {
            let dev_guard = self.connected_dev.lock().await;
            dev_guard
                .as_ref()
                .cloned()
                .ok_or(Error::NoDeviceConnected)?
        };
        let charac = {
            let state = self.state.lock().await;
            state.get_charac(c)?.clone()
        };
        run_with_timeout(
            dev.unsubscribe(&charac),
            "unsubscribe",
            self.timeouts().subscribe,
        )
        .await?;
        let mut listeners = self.notify_listeners.lock().await;
        listeners.retain(|l| l.uuid != charac.uuid);
        Ok(())
    }

    pub(crate) async fn handle_event(&self, event: CentralEvent) -> Result<(), Error> {
        debug!("handling event: {event:?}");
        match event {
            CentralEvent::DeviceDisconnected(peripheral_id) => {
                self.handle_disconnect(peripheral_id).await?;
            }
            CentralEvent::DeviceConnected(peripheral_id) => {
                self.handle_connect(peripheral_id).await;
            }
            CentralEvent::StateUpdate(CentralState::PoweredOff) => {
                // Every link is gone with the adapter, and no disconnect event
                // will arrive for it.
                self.handle_adapter_powered_off().await;
            }

            _event => {}
        }
        Ok(())
    }

    async fn handle_adapter_powered_off(&self) {
        if self.connected_dev.lock().await.is_none() {
            return;
        }
        warn!("adapter powered off while connected, clearing connection state");
        self.clear_connection_state().await;
    }

    /// Returns the connected device
    /// # Errors
    /// Returns an error if no device is connected
    pub async fn connected_device(&self) -> Result<BleDevice, Error> {
        let p = self.connected_dev.lock().await;
        let p = p.as_ref().ok_or(Error::NoDeviceConnected)?;
        let d = BleDevice::from_peripheral(p).await?;
        Ok(d)
    }

    #[allow(clippy::redundant_closure_for_method_calls)]
    async fn handle_connect(&self, peripheral_id: PeripheralId) {
        let connected_device = self.connected_dev.lock().await.as_ref().map(|d| d.id());
        if let Some(connected_device) = connected_device {
            if connected_device == peripheral_id {
                trace!("handle_connect: DeviceConnected event for {peripheral_id}");
                debug!("connection to {peripheral_id} established");
                self.connected_tx
                    .send(true)
                    .expect("failed to send connected update");
                debug!("connected_tx updated");
                return;
            }
            error!("Unexpected connect event for device {peripheral_id}, connected device is {connected_device}");
        } else {
            // Nobody is waiting for this connection — typically a `connect()`
            // that already gave up, whose `connectGatt` completed afterwards.
            // Adopting it would leave `connected_tx` false with an open link.
            warn!("Connect event for {peripheral_id} without a pending connection");
        }
        self.disconnect_peripheral(&peripheral_id).await;
    }

    /// Disconnects a peripheral directly, bypassing the handler state. Used for
    /// links we never adopted, where [`Self::disconnect`] would not find a device.
    async fn disconnect_peripheral(&self, peripheral_id: &PeripheralId) {
        let peripheral = self
            .devices
            .lock()
            .await
            .values()
            .find(|p| p.id() == *peripheral_id)
            .cloned();
        let Some(peripheral) = peripheral else {
            error!("Cannot disconnect unknown device {peripheral_id}");
            return;
        };
        info!("disconnecting unexpected connection to {peripheral_id}");
        if let Err(e) = peripheral.disconnect().await {
            error!("Failed to disconnect from device {peripheral_id}: {e}");
        }
    }

    async fn send_connection_update(&self, state: bool) {
        let tx = &mut self.state.lock().await.connection_update_channel;
        info!("sending connection update to {} listeners", tx.len());
        let mut remove = vec![];
        for (i, t) in tx.iter_mut().enumerate() {
            if let Err(e) = t.send(state).await {
                warn!("Failed to send connection update: {e}");
                remove.push(i);
            }
        }
    }

    async fn send_scan_update(&self, state: bool) {
        let tx = &mut self.state.lock().await.scan_update_channel;
        let mut remove = vec![];
        for (i, t) in tx.iter_mut().enumerate() {
            if let Err(e) = t.send(state).await {
                warn!("Failed to send scan update: {e}");
                remove.push(i);
            }
        }
    }

    pub async fn get_adapter_state(&'static self) -> AdapterState {
        let adapter = match self.get_or_init_adapter().await {
            Ok(a) => a,
            Err(e) => {
                error!("Failed to init adapter for state check: {e}");
                return AdapterState::Unknown;
            }
        };

        match adapter.adapter_state().await {
            Ok(state) => match state {
                CentralState::Unknown => AdapterState::Unknown,
                CentralState::PoweredOn => AdapterState::On,
                CentralState::PoweredOff => AdapterState::Off,
            },
            Err(e) => {
                error!("Failed to get adapter state: {e}");
                AdapterState::Unknown
            }
        }
    }
}

async fn run_with_timeout<T: Send + Sync + 'static>(
    fut: impl Future<Output = Result<T, btleplug::Error>> + Send,
    cmd: &str,
    timeout: Duration,
) -> Result<T, Error> {
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| Error::Timeout(cmd.to_string()))?
        .map_err(Error::Btleplug)
}

async fn filter_peripherals(discovered: &mut Vec<Peripheral>, filter: &ScanFilter) {
    if matches!(filter, ScanFilter::None) {
        return;
    }
    let mut remove = vec![];
    for p in discovered.iter().enumerate() {
        let Ok(Some(properties)) = p.1.properties().await else {
            // can't filter without properties
            remove.push(p.0);
            continue;
        };
        if properties.rssi.is_none() {
            // ignore not available devices
            remove.push(p.0);
            continue;
        }
        match filter {
            ScanFilter::None => unreachable!("Earyl return for no filter"),
            ScanFilter::Service(uuid) => {
                if !properties.services.iter().any(|s| s == uuid) {
                    remove.push(p.0);
                }
            }
            ScanFilter::AnyService(uuids) => {
                if !properties.services.iter().any(|s| uuids.contains(s)) {
                    remove.push(p.0);
                }
            }
            ScanFilter::AllServices(uuids) => {
                if !uuids.iter().all(|s| properties.services.contains(s)) {
                    remove.push(p.0);
                }
            }
            ScanFilter::ManufacturerData(key, value) => {
                if !properties
                    .manufacturer_data
                    .get(key)
                    .is_some_and(|v| v == value)
                {
                    remove.push(p.0);
                }
            }
            ScanFilter::ManufacturerDataMasked(key, value, maks) => {
                let Some(data) = properties.manufacturer_data.get(key) else {
                    remove.push(p.0);
                    continue;
                };
                if !data
                    .iter()
                    .zip(maks.iter())
                    .zip(value.iter())
                    .all(|((d, m), v)| (d & m) == (*v & m))
                {
                    remove.push(p.0);
                }
            }
        }
    }

    for i in remove.iter().rev() {
        discovered.swap_remove(*i);
    }
}

async fn listen_notify(dev: Option<Peripheral>, listeners: Arc<Mutex<Vec<Listener>>>) {
    let mut stream = dev
        .expect("no device connected")
        .notifications()
        .await
        .expect("failed to get notifications stream");
    while let Some(data) = stream.next().await {
        debug!(
            "notification received, listeners: {}",
            listeners.lock().await.len()
        );
        for l in listeners.lock().await.iter() {
            if l.uuid == data.uuid && l.service == data.service_uuid {
                trace!(
                    "notification from {}/{}: {} bytes: {:02x?}",
                    data.service_uuid,
                    data.uuid,
                    data.value.len(),
                    data.value
                );
                // run callback
                trace!("starting callback for {:?}", l.uuid);
                l.callback.run(data.value.clone()).await;
                trace!("callback for {:?} finished", l.uuid);
            }
        }
    }
    info!("Notification stream ended");
}
