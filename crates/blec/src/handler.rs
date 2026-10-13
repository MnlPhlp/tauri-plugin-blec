use crate::connection::Connection;
use crate::error::Error;
use crate::models::{
    self, AdapterState, BleDevice, ConnectionUpdate, ScanFilter, Service, Timeouts,
};
use crate::ALLOW_IBEACONS;
use btleplug::api::{Central, Characteristic, Manager as _, Peripheral as _};
use btleplug::api::{CentralEvent, CentralState, RetrievePeripheralsOptions};
use btleplug::platform::PeripheralId;
use futures::{Stream, StreamExt};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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

/// Upper bound for the scan [`Handler::scan_for`] runs to find an address that
/// is not in the device index yet. It returns as soon as the address shows up.
const ADDRESS_SCAN_MS: u64 = 5000;

struct Listener {
    uuid: Uuid,
    service: Uuid,
    callback: SubscriptionHandler,
}

/// Source of [`Link::id`].
static NEXT_LINK_ID: AtomicU64 = AtomicU64::new(0);

/// Everything the handler knows about the link to one device.
///
/// A link exists from the moment a connect to its address starts until the
/// connection is gone; [`Link::connected`] tells the pending phase apart from
/// the established one.
struct Link {
    /// Tells this link apart from a later one to the same address, so a slow
    /// disconnect never clears a connection that was made after it started.
    id: u64,
    /// The instance `connect()` ran on. On android it carries the negotiated
    /// MTU, which a clone taken before the connect would not see.
    peripheral: Peripheral,
    /// Whether the adapter reported the link as up. Set by `handle_connect`,
    /// waited on by connect and disconnect.
    connected: watch::Sender<bool>,
    characs: Vec<Characteristic>,
    listeners: Arc<Mutex<Vec<Listener>>>,
    /// Task forwarding the device's notifications to `listeners`
    listen_task: Option<tokio::task::JoinHandle<()>>,
    on_disconnect: OnDisconnectHandler,
    /// Serializes the GATT operations on this device: only one read, write,
    /// subscription or discovery can be in flight per link. Operations on
    /// different devices run in parallel.
    gatt_lock: Arc<Mutex<()>>,
    /// A link [`Handler::discover_services`] opened only to read the services.
    /// It is invisible to the rest of the API and emits no connection updates.
    temporary: bool,
}

impl Link {
    fn new(peripheral: Peripheral, temporary: bool) -> Self {
        Self {
            id: NEXT_LINK_ID.fetch_add(1, Ordering::Relaxed),
            peripheral,
            connected: watch::channel(false).0,
            characs: vec![],
            listeners: Arc::new(Mutex::new(vec![])),
            listen_task: None,
            on_disconnect: OnDisconnectHandler::None,
            gatt_lock: Arc::new(Mutex::new(())),
            temporary,
        }
    }

    fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }

    fn get_charac(&self, uuid: Uuid, service: Option<Uuid>) -> Result<&Characteristic, Error> {
        trace!("getting characteristic {uuid} from service {service:?}");
        let charac = self
            .characs
            .iter()
            .find(|c| c.uuid == uuid && service.map_or(true, |s| c.service_uuid == s));
        charac.ok_or(Error::CharacNotAvailable(uuid.to_string()))
    }
}

struct HandlerState {
    connection_update_channel: Vec<mpsc::Sender<bool>>,
    connection_event_channel: Vec<mpsc::Sender<ConnectionUpdate>>,
    scan_update_channel: Vec<mpsc::Sender<bool>>,
    scan_task: Option<tokio::task::JoinHandle<()>>,
}

pub struct Handler {
    devices: Arc<Mutex<HashMap<String, Peripheral>>>,
    adapter: Mutex<Option<Arc<Adapter>>>,
    /// Task forwarding the adapter's `CentralEvent`s to `handle_event`
    event_pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The links by address. A std mutex, so the synchronous getters can read
    /// it; it is never held across an `.await`.
    links: std::sync::Mutex<HashMap<String, Link>>,
    /// Serializes connection attempts across devices: android's `connectGatt`
    /// misbehaves with several connects in flight. Operations on links that
    /// are already up do not take it.
    connect_lock: Mutex<()>,
    state: Mutex<HandlerState>,
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
        // Finds the kotlin side and wires it up before anything tries to talk
        // to it. Idempotent, and a no-op when the host already did it through
        // `android::init_with`.
        #[cfg(all(target_os = "android", not(any(test, feature = "mock"))))]
        crate::android::init()?;
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
        Self {
            devices: Arc::new(Mutex::new(HashMap::new())),
            adapter: Mutex::new(adapter),
            event_pump: Mutex::new(None),
            links: std::sync::Mutex::new(HashMap::new()),
            connect_lock: Mutex::new(()),
            timeouts: std::sync::Mutex::new(Timeouts::default()),
            state: Mutex::new(HandlerState {
                connection_update_channel: vec![],
                connection_event_channel: vec![],
                scan_task: None,
                scan_update_channel: vec![],
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

    /// The link table. Never hold the guard across an `.await`.
    fn links(&self) -> std::sync::MutexGuard<'_, HashMap<String, Link>> {
        self.links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Returns true if at least one device is connected
    pub fn is_connected(&self) -> bool {
        self.links()
            .values()
            .any(|link| !link.temporary && link.is_connected())
    }

    /// Returns true if the device with this address is connected
    pub fn is_device_connected(&self, address: &str) -> bool {
        self.links()
            .get(address)
            .is_some_and(|link| !link.temporary && link.is_connected())
    }

    /// The address of the only device connected (or being connected), which
    /// the methods without an address parameter work on.
    ///
    /// # Errors
    /// [`Error::NoDeviceConnected`] without a device,
    /// [`Error::MultipleDevicesConnected`] with more than one.
    fn single_address(&self) -> Result<String, Error> {
        let links = self.links();
        let mut addresses = links
            .iter()
            .filter(|(_, link)| !link.temporary)
            .map(|(address, _)| address);
        match (addresses.next(), addresses.next()) {
            (None, _) => Err(Error::NoDeviceConnected),
            (Some(address), None) => Ok(address.clone()),
            (Some(_), Some(_)) => Err(Error::MultipleDevicesConnected),
        }
    }

    /// The peripheral of the link to `address`.
    fn link_peripheral(&self, address: &str) -> Result<Peripheral, Error> {
        self.links()
            .get(address)
            .filter(|link| !link.temporary)
            .map(|link| link.peripheral.clone())
            .ok_or_else(|| Error::DeviceNotConnected(address.to_string()))
    }

    /// The lock serializing the GATT operations on the link to `address`.
    fn link_gatt_lock(&self, address: &str) -> Result<Arc<Mutex<()>>, Error> {
        self.links()
            .get(address)
            .filter(|link| !link.temporary)
            .map(|link| link.gatt_lock.clone())
            .ok_or_else(|| Error::DeviceNotConnected(address.to_string()))
    }

    /// The peripheral and characteristic for an operation on `address`.
    fn link_charac(
        &self,
        address: &str,
        c: Uuid,
        service: Option<Uuid>,
    ) -> Result<(Peripheral, Characteristic), Error> {
        let links = self.links();
        let link = links
            .get(address)
            .filter(|link| !link.temporary)
            .ok_or_else(|| Error::DeviceNotConnected(address.to_string()))?;
        let charac = link.get_charac(c, service)?.clone();
        Ok((link.peripheral.clone(), charac))
    }

    /// Returns mtu (Maximum Transfer Unit) of the connected device
    /// # Errors
    /// Returns an error if no device or more than one device is connected
    pub async fn mtu(&self) -> Result<u16, Error> {
        self.device_mtu(&self.single_address()?).await
    }

    /// Returns mtu (Maximum Transfer Unit) of the device with this address
    /// # Errors
    /// Returns an error if the device is not connected
    pub async fn device_mtu(&self, address: &str) -> Result<u16, Error> {
        Ok(self.link_peripheral(address)?.mtu())
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
    /// use tokio::sync::mpsc;
    /// # async fn example() {
    ///     let handler = blec::get_handler().unwrap();
    ///     let (tx, mut rx) = mpsc::channel(1);
    ///     handler.set_scanning_update_channel(tx).await;
    ///     while let Some(scanning) = rx.recv().await {
    ///         println!("Scanning: {scanning}");
    ///     }
    /// # }
    /// ```
    pub async fn set_scanning_update_channel(&self, tx: mpsc::Sender<bool>) {
        self.state.lock().await.scan_update_channel.push(tx);
    }

    /// Takes a sender that will be used to send changes in the connection status
    ///
    /// After every connect and disconnect of any device it receives whether at
    /// least one device is connected. Use
    /// [`set_connection_event_channel`](Self::set_connection_event_channel)
    /// to learn which device it was.
    /// # Example
    /// ```no_run
    /// use tokio::sync::mpsc;
    /// # async fn example() {
    ///     let handler = blec::get_handler().unwrap();
    ///     let (tx, mut rx) = mpsc::channel(1);
    ///     handler.set_connection_update_channel(tx).await;
    ///     while let Some(connected) = rx.recv().await {
    ///         println!("Connected: {connected}");
    ///     }
    /// # }
    /// ```
    pub async fn set_connection_update_channel(&self, tx: mpsc::Sender<bool>) {
        self.state.lock().await.connection_update_channel.push(tx);
    }

    /// Takes a sender that will receive a [`ConnectionUpdate`] whenever a
    /// device connects or disconnects
    /// # Example
    /// ```no_run
    /// use tokio::sync::mpsc;
    /// # async fn example() {
    ///     let handler = blec::get_handler().unwrap();
    ///     let (tx, mut rx) = mpsc::channel(8);
    ///     handler.set_connection_event_channel(tx).await;
    ///     while let Some(update) = rx.recv().await {
    ///         println!("{}: connected {}", update.address, update.connected);
    ///     }
    /// # }
    /// ```
    pub async fn set_connection_event_channel(&self, tx: mpsc::Sender<ConnectionUpdate>) {
        self.state.lock().await.connection_event_channel.push(tx);
    }

    /// Connects to the given address
    ///
    /// Devices that are already connected stay connected, so several devices
    /// can be connected at the same time. Connecting to a device that is
    /// already connected keeps the connection and replaces its disconnect
    /// callback.
    ///
    /// If a callback is provided, it will be called when the device is disconnected.
    /// Because connecting sometimes fails especially on android, this method tries up to 3 times
    /// before returning an error
    ///
    /// Returns a [`Connection`] to use the device with.
    /// # Errors
    /// Returns an error if no devices are found, if the connection fails, or if
    /// the service/characteristics discovery fails
    /// # Example
    /// ```no_run
    /// use blec::OnDisconnectHandler;
    /// # async fn example() {
    ///    let handler = blec::get_handler().unwrap();
    ///    let device = handler.connect("00:00:00:00:00:00", OnDisconnectHandler::from_sync(|| println!("disconnected")), false).await.unwrap();
    ///    println!("mtu: {}", device.mtu().await.unwrap());
    /// # }
    /// ```
    pub async fn connect(
        &'static self,
        address: &str,
        on_disconnect: OnDisconnectHandler,
        allow_ibeacons: bool,
    ) -> Result<Connection, Error> {
        let _connecting = self.connect_lock.lock().await;
        let mut device = self.resolve_peripheral(address, allow_ibeacons).await?;
        // cancel any running discovery
        let _ = self.stop_scan().await;
        // connect to the given address
        // try up to 3 times before returning an error
        let mut connected = Ok(());
        for i in 0..3 {
            if let Err(e) = self.connect_device(address, device.clone(), false).await {
                if i < 2 {
                    warn!("Failed to connect device, retrying in 1s: {e}");
                    sleep(Duration::from_secs(1)).await;
                    // A failed connect can be the backend having dropped its
                    // record of the peripheral, so ask for a fresh one instead
                    // of retrying with the handle that just failed.
                    device = self.reregister(address, device).await;
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
            if let Ok(device) = self.link_peripheral(address) {
                if let Err(e) = device.disconnect().await {
                    warn!("Failed to cancel the pending connection: {e}");
                }
            }
            self.clear_link(address, None).await;
            return Err(e);
        }
        debug!("connecting services");
        // discover service/characteristics (no lock held during GATT op)
        let characs = match self.connect_services(address).await {
            Ok(characs) => characs,
            Err(e) => {
                // The link is up but unusable — tear it down instead of
                // returning with a connected device the caller doesn't know about.
                error!("Failed to discover services: {e}");
                if let Err(e) = self.disconnect_device(address).await {
                    warn!("Failed to disconnect after failed service discovery: {e}");
                }
                return Err(e);
            }
        };
        {
            let mut links = self.links();
            let Some(link) = links.get_mut(address) else {
                // the link dropped during service discovery and is cleared already
                warn!("{address} disconnected during service discovery");
                return Err(Error::ConnectionFailed);
            };
            // set callback to run on disconnect
            link.on_disconnect = on_disconnect;
            link.characs = characs;
            debug!("Starting notification task");
            // start background task for notifications, replacing the one of
            // a connection that is reused
            if let Some(task) = link.listen_task.take() {
                task.abort();
            }
            link.listen_task = Some(tokio::task::spawn(listen_notify(
                link.peripheral.clone(),
                link.listeners.clone(),
            )));
        }
        self.send_connection_update(address, true).await;
        info!("connecting to {address} done");
        Ok(Connection::new(self, address))
    }

    async fn connect_services(&self, address: &str) -> Result<Vec<Characteristic>, Error> {
        let (device, gatt_lock) = {
            let links = self.links();
            let link = links.get(address).ok_or(Error::NoDeviceConnected)?;
            (link.peripheral.clone(), link.gatt_lock.clone())
        };
        debug!("starting service discovery");
        {
            let _gatt_guard = gatt_lock.lock().await;
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

    /// The peripheral for `address`, one the backend still knows about.
    ///
    /// A device with a link uses the link's peripheral: a connected device
    /// usually stops advertising, so the next scan would forget it.
    ///
    /// Otherwise [`Self::devices`] maps addresses to the peripheral a scan last reported
    /// for them, and that mapping outlives the backend's own record of the
    /// peripheral: CoreBluetooth forgets a peripheral as soon as its link goes
    /// down, and the android side clears its device map whenever a scan starts.
    /// The stale handle is still listed by `peripherals()`, so a device the user
    /// connected to once looks perfectly available while every connect fails
    /// with "Peripheral no longer available" until a new advertisement arrives.
    /// Have the backend hand out a peripheral for the id instead of trusting the
    /// index, and only fall back to a scan when the address is unknown entirely.
    async fn resolve_peripheral(
        &'static self,
        address: &str,
        allow_ibeacons: bool,
    ) -> Result<Peripheral, Error> {
        let linked = self
            .links()
            .get(address)
            .map(|link| link.peripheral.clone());
        if let Some(device) = linked {
            return Ok(device);
        }
        let known = self.devices.lock().await.get(address).cloned();
        let device = match known {
            Some(device) => device,
            None => {
                self.scan_for(address, allow_ibeacons).await;
                self.devices
                    .lock()
                    .await
                    .get(address)
                    .cloned()
                    .ok_or_else(|| Error::UnknownPeripheral(address.to_string()))?
            }
        };
        Ok(self.reregister(address, device).await)
    }

    /// Asks the backend to re-register the peripheral and puts whatever it
    /// hands back into the device index.
    ///
    /// Best effort: a backend without a retrieval source (see
    /// [`Central::retrieve_peripherals`]) or one that does not know the id keeps
    /// `device`, which is still the caller's best guess.
    async fn reregister(&'static self, address: &str, device: Peripheral) -> Peripheral {
        let Ok(adapter) = self.get_or_init_adapter().await else {
            return device;
        };
        let id = device.id();
        let retrieved = adapter
            .retrieve_peripherals(RetrievePeripheralsOptions {
                identifiers: Some(vec![id.clone()]),
                services: None,
            })
            .await;
        let found = match retrieved {
            Ok(peripherals) => peripherals.into_iter().find(|p| p.id() == id),
            Err(btleplug::Error::NotSupported(_)) => return device,
            Err(e) => {
                warn!("Failed to retrieve {address} from the adapter: {e}");
                return device;
            }
        };
        let Some(found) = found else {
            warn!("adapter does not know {address} ({id}) anymore");
            return device;
        };
        debug!("re-registered {address} with the adapter");
        self.devices
            .lock()
            .await
            .insert(address.to_string(), found.clone());
        found
    }

    /// Scans until `address` shows up or [`ADDRESS_SCAN_MS`] elapsed, so that
    /// the device index knows the address. Best effort: errors are logged.
    async fn scan_for(&'static self, address: &str, allow_ibeacons: bool) {
        debug!("{address} is unknown, scanning for it");
        let (tx, mut rx) = mpsc::channel(8);
        if let Err(e) = self
            .discover(Some(tx), ADDRESS_SCAN_MS, ScanFilter::None, allow_ibeacons)
            .await
        {
            warn!("Failed to scan for {address}: {e}");
            return;
        }
        while let Some(devices) = rx.recv().await {
            if devices.iter().any(|dev| dev.address == address) {
                break;
            }
        }
        // The scan task panics if it cannot deliver a snapshot, so it has to be
        // stopped before the receiver goes away, and whatever it queued in the
        // meantime has to be drained.
        let _ = self.stop_scan().await;
        while rx.recv().await.is_some() {}
    }

    /// Opens the link to `address`, registering it before the connect so
    /// `handle_connect` can match the adapter's event to it.
    async fn connect_device(
        &self,
        address: &str,
        device: Peripheral,
        temporary: bool,
    ) -> Result<(), Error> {
        trace!("connect_device: initiating connection to {address}");
        debug!("connecting to {address}",);
        let (mut connected_rx, gatt_lock) = {
            let mut links = self.links();
            let link = links
                .entry(address.to_string())
                .or_insert_with(|| Link::new(device.clone(), temporary));
            link.peripheral = device.clone();
            (link.connected.subscribe(), link.gatt_lock.clone())
        };
        if device.is_connected().await? {
            debug!("Device already connected");
            self.set_link_connected(address, device);
            return Ok(());
        }
        debug!("Connecting to device");
        let timeout = self.timeouts().connect;
        {
            let _gatt_guard = gatt_lock.lock().await;
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
        if !*connected_rx.borrow() {
            // still not connected
            warn!("Still not connected after connection event");
            return Err(Error::ConnectionFailed);
        }
        // keep the instance `connect()` ran on, see `Link::peripheral`
        if let Some(link) = self.links().get_mut(address) {
            link.peripheral = device;
        }
        trace!("connect_device: connection established to {address}");
        info!("device connected");
        Ok(())
    }

    /// Marks the link to `address` as up, adopting `device` as its peripheral.
    fn set_link_connected(&self, address: &str, device: Peripheral) {
        if let Some(link) = self.links().get_mut(address) {
            link.peripheral = device;
            link.connected.send_replace(true);
        }
    }

    /// Drops every trace of the link to `address`: stops its notification
    /// task, runs its disconnect callback and reports the new state to all
    /// listeners. With `only`, a link with a different id is left alone.
    async fn clear_link(&self, address: &str, only: Option<u64>) {
        let link = {
            let mut links = self.links();
            if only.is_some_and(|id| links.get(address).is_some_and(|link| link.id != id)) {
                debug!("{address} was reconnected meanwhile, keeping the new link");
                return;
            }
            links.remove(address)
        };
        let Some(mut link) = link else {
            return;
        };
        if let Some(task) = link.listen_task.take() {
            task.abort();
        }
        link.on_disconnect.take().run().await;
        if !link.temporary {
            self.send_connection_update(address, false).await;
        }
        // last, so whoever waits for the disconnect finds everything above done
        link.connected.send_replace(false);
    }

    /// Disconnects from the connected device
    ///
    /// # Errors
    /// Returns an error if no device or more than one device is connected,
    /// see [`disconnect_device`](Self::disconnect_device) for the rest.
    pub async fn disconnect(&self) -> Result<(), Error> {
        self.disconnect_device(&self.single_address()?).await
    }

    /// Disconnects every connected device
    ///
    /// # Errors
    /// Returns the first error a disconnect failed with, after trying all of them
    pub async fn disconnect_all(&self) -> Result<(), Error> {
        let addresses: Vec<String> = self
            .links()
            .iter()
            .filter(|(_, link)| !link.temporary)
            .map(|(address, _)| address.clone())
            .collect();
        let mut result = Ok(());
        for address in addresses {
            if let Err(e) = self.disconnect_device(&address).await {
                warn!("Failed to disconnect {address}: {e}");
                if result.is_ok() {
                    result = Err(e);
                }
            }
        }
        result
    }

    /// Disconnects from the device with this address
    /// This triggers a disconnect and then waits for the actual disconnect event from the adapter.
    ///
    /// The disconnect is always attempted on the peripheral, even when the
    /// internal state says it is no longer connected: on Android that state can
    /// be out of sync with a still open GATT link. A state desync is therefore
    /// repaired instead of reported, and only a missing device is an error.
    /// # Errors
    /// Returns an error if the device is not connected
    pub async fn disconnect_device(&self, address: &str) -> Result<(), Error> {
        trace!("disconnect: user-initiated disconnect of {address}");
        info!("disconnect of {address} triggered by user");
        // Clone: the lock must not be held while waiting for the disconnect event
        let link = self
            .links()
            .get(address)
            .filter(|link| !link.temporary)
            .map(|link| (link.id, link.peripheral.clone(), link.connected.subscribe()));
        let Some((id, dev, mut connected_rx)) = link else {
            debug!("{address} not connected");
            return Err(Error::DeviceNotConnected(address.to_string()));
        };
        let was_connected = *connected_rx.borrow_and_update();
        let is_connected = dev.is_connected().await.unwrap_or(false);
        if let Err(e) = dev.disconnect().await {
            warn!("Failed to trigger disconnect: {e}");
        }
        if !is_connected || !was_connected {
            // No disconnect event will arrive because the adapter doesn't
            // consider the device connected anymore. Repair the state.
            warn!("device was already disconnected, clearing connection state");
            self.clear_link(address, Some(id)).await;
            return Ok(());
        }
        // the change will be triggered by handle_event -> handle_disconnect which runs in another
        // task
        let timeout = self.timeouts().disconnect;
        match tokio::time::timeout(timeout, connected_rx.changed()).await {
            Ok(_) if !*connected_rx.borrow() => return Ok(()),
            Ok(Ok(())) => warn!("still connected after the disconnect event"),
            Ok(Err(e)) => warn!("disconnect event channel closed: {e}"),
            Err(_) => warn!("timed out waiting for the disconnect event"),
        }
        // The platform side closes the GATT link on its own; make sure our
        // state agrees so the next connect isn't blocked by a stale device.
        self.clear_link(address, Some(id)).await;
        Ok(())
    }

    /// The address of the link to the peripheral with this id.
    fn link_address(&self, peripheral_id: &PeripheralId) -> Option<String> {
        self.links()
            .iter()
            .find(|(_, link)| link.peripheral.id() == *peripheral_id)
            .map(|(address, _)| address.clone())
    }

    /// Clears internal state, updates connected flag and calls disconnect callback
    async fn handle_disconnect(&self, peripheral_id: PeripheralId) -> Result<(), Error> {
        trace!("handle_disconnect: DeviceDisconnected event for {peripheral_id}");
        info!("Handling disconnect for {peripheral_id}");
        let Some(address) = self.link_address(&peripheral_id) else {
            // event not for a device we are connected to, ignore
            warn!("Unexpected disconnect event for device {peripheral_id}, no link to it");
            return Ok(());
        };
        info!("disconnecting {address}");
        self.clear_link(&address, None).await;
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
    /// use tokio::sync::mpsc;
    /// use blec::models::ScanFilter;
    ///
    /// # async fn example() {
    ///     let handler = blec::get_handler().unwrap();
    ///     let (tx, mut rx) = mpsc::channel(1);
    ///     handler.discover(Some(tx),1000, ScanFilter::None, false).await.unwrap();
    ///     while let Some(devices) = rx.recv().await {
    ///         println!("Discovered {devices:?}");
    ///     }
    /// # }
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
    /// An address that is not known yet is scanned for, like [`Self::connect`] does
    ///
    /// The connection made only for the discovery is not reported as a
    /// connection and does not affect the other connected devices.
    /// # Errors
    /// Returns an error if the device is not found, if the connection fails, or if the discovery fails
    pub async fn discover_services(&'static self, address: &str) -> Result<Vec<Service>, Error> {
        let linked = self
            .links()
            .get(address)
            .filter(|link| !link.temporary && link.is_connected())
            .map(|link| (link.peripheral.clone(), link.gatt_lock.clone()));
        if let Some((device, gatt_lock)) = linked {
            debug!("discovering services on connected {address}");
            return self.read_services(&device, &gatt_lock).await;
        }

        let _connecting = self.connect_lock.lock().await;
        let device = self.resolve_peripheral(address, false).await?;
        if device.is_connected().await? {
            // connected, but not by us: leave it that way
            return self.read_services(&device, &Arc::new(Mutex::new(()))).await;
        }
        if let Err(e) = self.connect_device(address, device.clone(), true).await {
            error!("Failed to connect for discovery: {e}");
            self.clear_link(address, None).await;
            return Err(e);
        }
        let (id, device, gatt_lock, mut connected_rx) = {
            let links = self.links();
            let link = links.get(address).ok_or(Error::ConnectionFailed)?;
            (
                link.id,
                link.peripheral.clone(),
                link.gatt_lock.clone(),
                link.connected.subscribe(),
            )
        };
        debug!("discovering services on {address}");
        let services = self.read_services(&device, &gatt_lock).await;
        if *connected_rx.borrow_and_update() {
            if let Err(e) = device.disconnect().await {
                warn!("Failed to disconnect after the discovery: {e}");
            }
            debug!("waiting for disconnect event");
            let timeout = self.timeouts().disconnect;
            if tokio::time::timeout(timeout, connected_rx.changed())
                .await
                .is_err()
            {
                warn!("timed out waiting for the disconnect event after the discovery");
            }
        }
        self.clear_link(address, Some(id)).await;
        services
    }

    /// The services of `device`, discovering them first if needed.
    async fn read_services(
        &self,
        device: &Peripheral,
        gatt_lock: &Mutex<()>,
    ) -> Result<Vec<Service>, Error> {
        if device.services().is_empty() {
            let _gatt_guard = gatt_lock.lock().await;
            run_with_timeout(
                device.discover_services(),
                "discover services",
                self.timeouts().discover_services,
            )
            .await?;
        }
        Ok(device.services().iter().map(Service::from).collect())
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
    /// Returns an error if no device or more than one device is connected,
    /// see [`send_data_to`](Self::send_data_to) for the rest
    /// # Example
    /// ```no_run
    /// use uuid::{Uuid,uuid};
    /// use blec::models::WriteType;
    ///
    /// const CHARACTERISTIC_UUID: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");
    /// # async fn example() {
    ///     let handler = blec::get_handler().unwrap();
    ///     let data = [1,2,3,4,5];
    ///     let response = handler.send_data(CHARACTERISTIC_UUID, None, &data, WriteType::WithResponse).await.unwrap();
    /// # }
    /// ```
    pub async fn send_data(
        &self,
        c: Uuid,
        service: Option<Uuid>,
        data: &[u8],
        write_type: models::WriteType,
    ) -> Result<(), Error> {
        self.send_data_to(&self.single_address()?, c, service, data, write_type)
            .await
    }

    /// Sends data to the given characteristic of the device with this address
    /// # Errors
    /// Returns an error if the device is not connected or the characteristic is not available
    /// or if the write operation fails
    pub async fn send_data_to(
        &self,
        address: &str,
        c: Uuid,
        service: Option<Uuid>,
        data: &[u8],
        write_type: models::WriteType,
    ) -> Result<(), Error> {
        let gatt_lock = self.link_gatt_lock(address)?;
        let _gatt_guard = gatt_lock.lock().await;
        let (dev, charac) = self.link_charac(address, c, service)?;
        trace!(
            "sending {} bytes to characteristic {c} of {address}: {:02x?}",
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
    /// Returns an error if no device or more than one device is connected,
    /// see [`recv_data_from`](Self::recv_data_from) for the rest
    /// # Example
    /// ```no_run
    /// use uuid::{Uuid,uuid};
    /// const CHARACTERISTIC_UUID: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");
    /// # async fn example() {
    ///     let handler = blec::get_handler().unwrap();
    ///     let response = handler.recv_data(CHARACTERISTIC_UUID, None).await.unwrap();
    /// # }
    /// ```
    pub async fn recv_data(&self, c: Uuid, service: Option<Uuid>) -> Result<Vec<u8>, Error> {
        self.recv_data_from(&self.single_address()?, c, service)
            .await
    }

    /// Receives data from the given characteristic of the device with this address
    /// Returns the data as a vector of bytes
    /// # Errors
    /// Returns an error if the device is not connected or the characteristic is not available
    /// or if the read operation fails
    pub async fn recv_data_from(
        &self,
        address: &str,
        c: Uuid,
        service: Option<Uuid>,
    ) -> Result<Vec<u8>, Error> {
        let gatt_lock = self.link_gatt_lock(address)?;
        let _gatt_guard = gatt_lock.lock().await;
        let (dev, charac) = self.link_charac(address, c, service)?;
        let data = run_with_timeout(dev.read(&charac), "read", self.timeouts().read).await?;
        trace!(
            "received {} bytes from characteristic {c} of {address}: {:02x?}",
            data.len(),
            data
        );
        Ok(data)
    }

    /// Subscribe to notifications from the given characteristic of the connected device
    /// The callback will be called whenever a notification is received
    /// # Errors
    /// Returns an error if no device or more than one device is connected,
    /// see [`subscribe_to`](Self::subscribe_to) for the rest
    /// # Example
    /// ```no_run
    /// use uuid::{Uuid,uuid};
    /// const CHARACTERISTIC_UUID: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");
    /// # async fn example() {
    ///     let handler = blec::get_handler().unwrap();
    ///     let response = handler.subscribe(CHARACTERISTIC_UUID, None, |data| println!("received {data:?}")).await.unwrap();
    /// # }
    /// ```
    pub async fn subscribe(
        &self,
        c: Uuid,
        service: Option<Uuid>,
        callback: impl Into<SubscriptionHandler>,
    ) -> Result<(), Error> {
        self.subscribe_to(&self.single_address()?, c, service, callback)
            .await
    }

    /// Subscribe to notifications from the given characteristic of the device with this address
    /// The callback will be called whenever a notification is received, until
    /// the device disconnects
    /// # Errors
    /// Returns an error if the device is not connected or the characteristic is not available
    /// or if the subscribe operation fails
    pub async fn subscribe_to(
        &self,
        address: &str,
        c: Uuid,
        service: Option<Uuid>,
        callback: impl Into<SubscriptionHandler>,
    ) -> Result<(), Error> {
        let gatt_lock = self.link_gatt_lock(address)?;
        let _gatt_guard = gatt_lock.lock().await;
        let (dev, charac) = self.link_charac(address, c, service)?;
        info!("subscribing to characteristic {charac:?} of {address}");
        run_with_timeout(
            dev.subscribe(&charac),
            "subscribe",
            self.timeouts().subscribe,
        )
        .await?;
        info!("subscribed successfully");
        let listeners = self
            .links()
            .get(address)
            .map(|link| link.listeners.clone())
            .ok_or_else(|| Error::DeviceNotConnected(address.to_string()))?;
        listeners.lock().await.push(Listener {
            uuid: charac.uuid,
            service: charac.service_uuid,
            callback: callback.into(),
        });
        Ok(())
    }

    /// Unsubscribe from notifications for the given characteristic of the connected device
    /// This will also remove the callback from the list of listeners
    /// # Errors
    /// Returns an error if no device or more than one device is connected,
    /// see [`unsubscribe_from`](Self::unsubscribe_from) for the rest
    pub async fn unsubscribe(&self, c: Uuid) -> Result<(), Error> {
        self.unsubscribe_from(&self.single_address()?, c, None)
            .await
    }

    /// Unsubscribe from notifications for the given characteristic of the device with this address
    /// This will also remove the callbacks from the list of listeners. Without
    /// a `service`, the callbacks of the characteristic in every service are removed.
    /// # Errors
    /// Returns an error if the device is not connected or the characteristic is not available
    /// or if the unsubscribe operation fails
    pub async fn unsubscribe_from(
        &self,
        address: &str,
        c: Uuid,
        service: Option<Uuid>,
    ) -> Result<(), Error> {
        let gatt_lock = self.link_gatt_lock(address)?;
        let _gatt_guard = gatt_lock.lock().await;
        let (dev, charac) = self.link_charac(address, c, service)?;
        run_with_timeout(
            dev.unsubscribe(&charac),
            "unsubscribe",
            self.timeouts().subscribe,
        )
        .await?;
        let listeners = self.links().get(address).map(|link| link.listeners.clone());
        if let Some(listeners) = listeners {
            listeners
                .lock()
                .await
                .retain(|l| l.uuid != charac.uuid || service.is_some_and(|s| l.service != s));
        }
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
        let addresses: Vec<String> = self.links().keys().cloned().collect();
        if addresses.is_empty() {
            return;
        }
        warn!("adapter powered off while connected, clearing connection state");
        for address in addresses {
            self.clear_link(&address, None).await;
        }
    }

    /// Returns the connected device
    /// # Errors
    /// Returns an error if no device or more than one device is connected
    pub async fn connected_device(&self) -> Result<BleDevice, Error> {
        let p = self.link_peripheral(&self.single_address()?)?;
        let d = BleDevice::from_peripheral(&p).await?;
        Ok(d)
    }

    /// Returns the connected device with this address
    /// # Errors
    /// Returns an error if the device is not connected
    pub async fn connected_device_at(&self, address: &str) -> Result<BleDevice, Error> {
        let p = self.link_peripheral(address)?;
        let d = BleDevice::from_peripheral(&p).await?;
        Ok(d)
    }

    /// Returns all connected devices, sorted by address
    pub async fn connected_devices(&self) -> Vec<BleDevice> {
        let peripherals: Vec<Peripheral> = self
            .links()
            .values()
            .filter(|link| !link.temporary && link.is_connected())
            .map(|link| link.peripheral.clone())
            .collect();
        let mut devices = vec![];
        for p in peripherals {
            match BleDevice::from_peripheral(&p).await {
                Ok(device) => devices.push(device),
                Err(e) => warn!("Failed to read connected device: {e}"),
            }
        }
        devices.sort();
        devices
    }

    /// A [`Connection`] to the device with this address, for a device that
    /// was connected with [`connect`](Self::connect)
    /// # Errors
    /// Returns an error if the device is not connected
    pub fn connection(&'static self, address: &str) -> Result<Connection, Error> {
        self.link_peripheral(address)?;
        Ok(Connection::new(self, address))
    }

    #[allow(clippy::redundant_closure_for_method_calls)]
    async fn handle_connect(&self, peripheral_id: PeripheralId) {
        {
            let links = self.links();
            let link = links
                .iter()
                .find(|(_, link)| link.peripheral.id() == peripheral_id);
            if let Some((address, link)) = link {
                trace!("handle_connect: DeviceConnected event for {peripheral_id}");
                debug!("connection to {address} ({peripheral_id}) established");
                link.connected.send_replace(true);
                return;
            }
        }
        // Nobody is waiting for this connection — typically a `connect()`
        // that already gave up, whose `connectGatt` completed afterwards.
        // Adopting it would leave the handler unaware of an open link.
        warn!("Connect event for {peripheral_id} without a pending connection");
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

    /// Reports that the device at `address` connected or disconnected.
    async fn send_connection_update(&self, address: &str, connected: bool) {
        let any_connected = connected || self.is_connected();
        let update = ConnectionUpdate {
            address: address.to_string(),
            connected,
        };
        let mut state = self.state.lock().await;
        info!(
            "sending connection update to {} listeners",
            state.connection_update_channel.len() + state.connection_event_channel.len()
        );
        for t in &state.connection_update_channel {
            if let Err(e) = t.send(any_connected).await {
                warn!("Failed to send connection update: {e}");
            }
        }
        for t in &state.connection_event_channel {
            if let Err(e) = t.send(update.clone()).await {
                warn!("Failed to send connection event: {e}");
            }
        }
        state.connection_update_channel.retain(|t| !t.is_closed());
        state.connection_event_channel.retain(|t| !t.is_closed());
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

async fn listen_notify(dev: Peripheral, listeners: Arc<Mutex<Vec<Listener>>>) {
    let mut stream = match dev.notifications().await {
        Ok(stream) => stream,
        Err(e) => {
            error!("failed to get notifications stream: {e}");
            return;
        }
    };
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
