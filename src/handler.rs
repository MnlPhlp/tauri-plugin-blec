use crate::error::Error;
use crate::models::{self, fmt_addr, AdapterState, BleDevice, ScanFilter, Service};
use crate::ALLOW_IBEACONS;
use btleplug::api::{Central, Characteristic, Manager as _, Peripheral as _};
use btleplug::api::{CentralEvent, CentralState};
use btleplug::platform::PeripheralId;
use futures::{Stream, StreamExt};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tauri::async_runtime;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::sleep;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

#[cfg(target_os = "android")]
use crate::android::{Adapter, Manager, Peripheral};
#[cfg(not(target_os = "android"))]
use btleplug::platform::{Adapter, Manager, Peripheral};

struct Listener {
    uuid: Uuid,
    service: Uuid,
    callback: SubscriptionHandler,
}

struct HandlerState {
    characs: Vec<Characteristic>,
    listen_handle: Option<async_runtime::JoinHandle<()>>,
    on_disconnect: OnDisconnectHandler,
    connection_update_channel: Vec<mpsc::Sender<bool>>,
    scan_update_channel: Vec<mpsc::Sender<bool>>,
    scan_task: Option<tokio::task::JoinHandle<()>>,
}

impl HandlerState {
    fn get_charac(&self, uuid: Uuid) -> Result<&Characteristic, Error> {
        info!("getting characteristic {uuid}");
        let charac = self.characs.iter().find(|c| c.uuid == uuid);
        charac.ok_or(Error::CharacNotAvailable(uuid.to_string()))
    }

    fn get_charac_from_service(&self, uuid: Uuid, service: Uuid) -> Result<&Characteristic, Error> {
        info!("getting characteristic {uuid} from service {service}");
        let charac = self
            .characs
            .iter()
            .find(|c| c.uuid == uuid && c.service_uuid == service);
        charac.ok_or(Error::CharacNotAvailable(uuid.to_string()))
    }
}

pub struct Handler {
    devices: Arc<Mutex<HashMap<String, Peripheral>>>,
    adapter: Arc<Adapter>,
    notify_listeners: Arc<Mutex<Vec<Listener>>>,
    connected_rx: watch::Receiver<bool>,
    connected_tx: watch::Sender<bool>,
    state: Mutex<HandlerState>,
    connected_dev: Mutex<Option<Peripheral>>,
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
    Async(futures::future::BoxFuture<'static, ()>),
}
impl OnDisconnectHandler {
    async fn run(self) {
        match self {
            OnDisconnectHandler::None => {}
            OnDisconnectHandler::Sync(f) => f(),
            OnDisconnectHandler::Async(f) => f.await,
        }
    }

    #[must_use]
    pub fn take(&mut self) -> Self {
        std::mem::replace(self, OnDisconnectHandler::None)
    }
}

impl<F: FnOnce() + Send + 'static> From<F> for OnDisconnectHandler {
    fn from(func: F) -> Self {
        OnDisconnectHandler::Sync(Box::new(func))
    }
}

impl From<futures::future::BoxFuture<'static, ()>> for OnDisconnectHandler {
    fn from(future: futures::future::BoxFuture<'static, ()>) -> Self {
        OnDisconnectHandler::Async(future)
    }
}

#[derive(Clone)]
#[allow(clippy::type_complexity)]
pub enum SubscriptionHandler {
    Sync(Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>),
    ASync(
        Arc<dyn (Fn(Vec<u8>) -> futures::future::BoxFuture<'static, ()>) + Send + Sync + 'static>,
    ),
}

impl SubscriptionHandler {
    pub fn from_async(
        func: impl (Fn(Vec<u8>) -> futures::future::BoxFuture<'static, ()>) + Send + Sync + 'static,
    ) -> Self {
        SubscriptionHandler::ASync(Arc::new(func))
    }

    async fn run(self, data: Vec<u8>) {
        match self {
            SubscriptionHandler::Sync(f) => tokio::task::spawn_blocking(move || f(data))
                .await
                .expect("failed to run sync callback"),
            SubscriptionHandler::ASync(f) => f(data).await,
        }
    }
}

impl<F: Fn(Vec<u8>) + Send + Sync + 'static> From<F> for SubscriptionHandler {
    fn from(func: F) -> Self {
        SubscriptionHandler::Sync(Arc::new(func))
    }
}

impl Handler {
    pub(crate) async fn new() -> Result<Self, Error> {
        let central = get_central().await?;
        let (connected_tx, connected_rx) = watch::channel(false);
        Ok(Self {
            devices: Arc::new(Mutex::new(HashMap::new())),
            adapter: Arc::new(central),
            notify_listeners: Arc::new(Mutex::new(vec![])),
            connected_rx,
            connected_tx,
            connected_dev: Mutex::new(None),
            state: Mutex::new(HandlerState {
                on_disconnect: OnDisconnectHandler::None,
                connection_update_channel: vec![],
                scan_task: None,
                scan_update_channel: vec![],
                listen_handle: None,
                characs: vec![],
            }),
        })
    }

    /// Returns true if a device is connected
    pub fn is_connected(&self) -> bool {
        *self.connected_rx.borrow()
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
    /// async_runtime::block_on(async {
    ///    let handler = tauri_plugin_blec::get_handler().unwrap();
    ///    handler.connect("00:00:00:00:00:00", (|| println!("disconnected")).into(), false).await.unwrap();
    /// });
    /// ```
    pub async fn connect(
        &'static self,
        address: &str,
        on_disconnect: OnDisconnectHandler,
        allow_ibeacons: bool,
    ) -> Result<(), Error> {
        if self.devices.lock().await.is_empty() {
            self.discover(None, 1000, ScanFilter::None, allow_ibeacons)
                .await?;
        }
        // cancel any running discovery
        let _ = self.stop_scan().await;
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
            *self.connected_dev.lock().await = None;
            let _ = self.connected_tx.send(false);
            error!("Failed to connect device: {e}");
            return Err(e);
        }
        {
            debug!("locking state");
            let mut state = self.state.lock().await;
            // set callback to run on disconnect
            state.on_disconnect = on_disconnect;
            debug!("connecting services");
            // discover service/characteristics
            self.connect_services(&mut state).await?;
            debug!("Starting notification task");
            // start background task for notifications
            state.listen_handle = Some(async_runtime::spawn(listen_notify(
                self.connected_dev.lock().await.clone(),
                self.notify_listeners.clone(),
            )));
        }
        self.send_connection_update(true).await;
        info!("connecting done");
        Ok(())
    }

    async fn connect_services(&self, state: &mut HandlerState) -> Result<(), Error> {
        let device = self.connected_dev.lock().await;
        let device = device.as_ref().ok_or(Error::NoDeviceConnected)?;
        debug!("starting service discovery");
        device.discover_services().await?;
        debug!("service discovery done");
        let services = device.services();
        for s in services {
            for c in &s.characteristics {
                state.characs.push(c.clone());
            }
        }
        Ok(())
    }

    async fn connect_device(&self, address: &str) -> Result<(), Error> {
        debug!("connecting to {address}",);
        let mut connected_rx = self.connected_rx.clone();
        let devices = self.devices.lock().await;
        let device = devices
            .get(address)
            .ok_or(Error::UnknownPeripheral(address.to_string()))?;
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
        run_with_timeout(device.connect(), "Connect").await?;
        // wait for the actual connection to be established
        if !*connected_rx.borrow_and_update() {
            info!("waiting for connection event");
            connected_rx
                .changed()
                .await
                .expect("failed to wait for connection event");
        }
        if !*self.connected_rx.borrow() {
            // still not connected
            warn!("Still not connected after connection event");
            return Err(Error::ConnectionFailed);
        }
        info!("device connected");
        Ok(())
    }

    /// Disconnects from the connected device
    /// This triggers a disconnect and then waits for the actual disconnect event from the adapter
    /// # Errors
    /// Returns an error if no device is connected or if the disconnect fails
    /// # Panics
    /// panics if there is an error with handling the internal disconnect event
    pub async fn disconnect(&self) -> Result<(), Error> {
        debug!("disconnect triggered by user");
        let mut connected_rx = self.connected_rx.clone();
        {
            // Scope is important to not lock device while waiting for disconnect event
            let dev = self.connected_dev.lock().await;
            if let Some(dev) = dev.as_ref() {
                if let Ok(true) = dev.is_connected().await {
                    assert!(
                        (*connected_rx.borrow_and_update()),
                        "connected_rx is false with a device being connected, this is a bug"
                    );
                    dev.disconnect().await?;
                } else {
                    debug!("device is not connected");
                    return Err(Error::NoDeviceConnected);
                }
            } else {
                debug!("no device connected");
                return Err(Error::NoDeviceConnected);
            }
        }
        debug!("waiting for disconnect event");
        // the change will be triggered by handle_event -> handle_disconnect which runs in another
        // task
        connected_rx
            .changed()
            .await
            .expect("failed to wait for disconnect event");
        if *self.connected_rx.borrow() {
            // still connected
            return Err(Error::DisconnectFailed);
        }
        Ok(())
    }

    /// Clears internal state, updates connected flag and calls disconnect callback
    async fn handle_disconnect(&self, peripheral_id: PeripheralId) -> Result<(), Error> {
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
        {
            debug!("locking state for disconnect");
            let mut state = self.state.lock().await;
            info!("disconnecting");
            *self.connected_dev.lock().await = None;
            if let Some(handle) = state.listen_handle.take() {
                handle.abort();
            }
            *self.notify_listeners.lock().await = vec![];
            state.on_disconnect.take().run().await;
            state.characs.clear();
        }
        self.send_connection_update(false).await;
        self.connected_tx
            .send(false)
            .expect("failed to send connected update");
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
        {
            let mut state = self.state.lock().await;
            // stop any ongoing scan
            if let Some(handle) = state.scan_task.take() {
                handle.abort();
                self.adapter.stop_scan().await?;
            }
            // start a new scan
            *ALLOW_IBEACONS.lock().await = allow_ibeacons;
            self.adapter
                .start_scan(btleplug::api::ScanFilter::default())
                .await?;
        }
        self.send_scan_update(true).await;
        let mut state = self.state.lock().await;
        let mut self_devices = self.devices.clone();
        let adapter = self.adapter.clone();
        state.scan_task = Some(tokio::task::spawn(async move {
            self_devices.lock().await.clear();
            let loops = timeout / 200;
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
            run_with_timeout(device.discover_services(), "discover services").await?;
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
    /// Returns an error if stopping the scan fails
    pub async fn stop_scan(&self) -> Result<(), Error> {
        self.adapter.stop_scan().await?;
        if let Some(handle) = self.state.lock().await.scan_task.take() {
            handle.abort();
        }
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
        let dev = self.connected_dev.lock().await;
        let dev = dev.as_ref().ok_or(Error::NoDeviceConnected)?;
        let state = self.state.lock().await;
        let charac = if let Some(service) = service {
            state.get_charac_from_service(c, service)?
        } else {
            state.get_charac(c)?
        };

        dev.write(charac, data, write_type.into()).await?;
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
        let dev = self.connected_dev.lock().await;
        let dev = dev.as_ref().ok_or(Error::NoDeviceConnected)?;
        let state = self.state.lock().await;
        let charac = if let Some(service) = service {
            state.get_charac_from_service(c, service)?
        } else {
            state.get_charac(c)?
        };
        let data = dev.read(charac).await?;
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
        let dev = self.connected_dev.lock().await;
        let dev = dev.as_ref().ok_or(Error::NoDeviceConnected)?;
        let state = self.state.lock().await;
        let charac = if let Some(service) = service {
            state.get_charac_from_service(c, service)?
        } else {
            state.get_charac(c)?
        };
        info!("subscribing to characteristic {charac:?}");
        dev.subscribe(charac).await?;
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
        let dev = self.connected_dev.lock().await;
        let dev = dev.as_ref().ok_or(Error::NoDeviceConnected)?;
        let state = self.state.lock().await;
        let charac = state.get_charac(c)?;
        dev.unsubscribe(charac).await?;
        let mut listeners = self.notify_listeners.lock().await;
        listeners.retain(|l| l.uuid != charac.uuid);
        Ok(())
    }

    pub(super) async fn get_event_stream(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = CentralEvent> + Send>>, Error> {
        let events = self.adapter.events().await?;
        Ok(events)
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

            _event => {}
        }
        Ok(())
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
                debug!("connection to {peripheral_id} established");
                self.connected_tx
                    .send(true)
                    .expect("failed to send connected update");
                debug!("connected_tx updated");
            } else {
                // event not for currently connected device, ignore
                warn!("Unexpected connect event for device {peripheral_id}, connected device is {connected_device}");
            }
        } else {
            warn!(
                "connect event for device {peripheral_id} received without waiting for connection"
            );
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

    pub async fn get_adapter_state(&self) -> AdapterState {
        match self.adapter.adapter_state().await {
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
) -> Result<T, Error> {
    tokio::time::timeout(Duration::from_secs(5), fut)
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
    let mut handles: HashMap<Uuid, tokio::task::JoinHandle<()>> = HashMap::new();
    while let Some(data) = stream.next().await {
        info!("notification received: {data:?}");
        for l in listeners.lock().await.iter() {
            if l.uuid == data.uuid && l.service == data.service_uuid {
                let cb = l.callback.clone();
                // wait for running callback first
                if let Some(handle) = handles.remove(&l.uuid) {
                    let _ = handle.await;
                    trace!("previous callback for {:?} finished", l.uuid);
                }
                // insert new callback
                trace!("starting new callback for {:?}", l.uuid);
                handles.insert(l.uuid, tokio::task::spawn(cb.run(data.value.clone())));
            }
        }
    }
}
