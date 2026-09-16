//! The simulated radio environment and the control API used by tests and apps.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use btleplug::api::{
    BDAddr, CentralEvent, CentralState, CharPropFlags, Characteristic, Service, ValueNotification,
    WriteType,
};
use btleplug::platform::PeripheralId;
use once_cell::sync::Lazy;
use tokio::sync::broadcast;
use uuid::Uuid;

use super::adapter::Adapter;
use super::id::peripheral_id;
use super::peripheral::Peripheral;

/// Capacity of the event/notification broadcast channels. Slow consumers
/// lose the oldest messages beyond this, like a real radio drops packets.
const CHANNEL_CAPACITY: usize = 4096;

static GLOBAL: Lazy<MockWorld> = Lazy::new(MockWorld::new);

/// A simulated Bluetooth environment: one adapter and any number of devices.
///
/// Cloning is cheap and every clone controls the same world.
#[derive(Clone, Debug)]
pub struct MockWorld {
    pub(super) inner: Arc<WorldInner>,
}

#[derive(Debug)]
pub(super) struct WorldInner {
    pub(super) events: broadcast::Sender<CentralEvent>,
    pub(super) devices: Mutex<Vec<Arc<DeviceInner>>>,
    pub(super) scanning: AtomicBool,
    pub(super) adapter_state: Mutex<CentralState>,
}

impl Default for MockWorld {
    fn default() -> Self {
        Self::new()
    }
}

impl MockWorld {
    /// Creates an empty, powered on world.
    #[must_use]
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(WorldInner {
                events,
                devices: Mutex::new(vec![]),
                scanning: AtomicBool::new(false),
                adapter_state: Mutex::new(CentralState::PoweredOn),
            }),
        }
    }

    /// The world the mock `Manager` hands out. This is what an app built with
    /// the `mock` feature talks to.
    pub fn global() -> &'static MockWorld {
        &GLOBAL
    }

    /// An adapter (btleplug `Central`) for this world.
    #[must_use]
    pub fn adapter(&self) -> Adapter {
        Adapter::new(self.clone())
    }

    /// Adds a device to the world. It is in range and disconnected.
    pub fn add_device(&self, spec: DeviceSpec) -> DeviceHandle {
        let inner = Arc::new(DeviceInner::new(spec, self.inner.events.clone()));
        self.inner.devices.lock().unwrap().push(inner.clone());
        DeviceHandle(inner)
    }

    /// Removes a device completely (no events are emitted).
    pub fn remove_device(&self, device: &DeviceHandle) {
        self.inner
            .devices
            .lock()
            .unwrap()
            .retain(|d| !Arc::ptr_eq(d, &device.0));
    }

    /// All devices in the world, in range or not.
    #[must_use]
    pub fn devices(&self) -> Vec<DeviceHandle> {
        self.inner
            .devices
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .map(DeviceHandle)
            .collect()
    }

    /// Changes the adapter state and emits the matching `StateUpdate` event.
    /// Powering off drops every link *without* per-device disconnect events,
    /// like a real adapter going away.
    pub fn set_adapter_state(&self, state: CentralState) {
        *self.inner.adapter_state.lock().unwrap() = state.clone();
        if state == CentralState::PoweredOff {
            self.inner.scanning.store(false, Ordering::Release);
            for device in self.inner.devices.lock().unwrap().iter() {
                device.drop_link(false);
            }
        }
        self.emit(CentralEvent::StateUpdate(state));
    }

    #[must_use]
    pub fn adapter_state(&self) -> CentralState {
        self.inner.adapter_state.lock().unwrap().clone()
    }

    /// Injects a raw event into the adapter's event stream.
    pub fn emit(&self, event: CentralEvent) {
        self.inner.emit(event);
    }

    /// Whether `start_scan` was called without a `stop_scan` since.
    #[must_use]
    pub fn is_scanning(&self) -> bool {
        self.inner.scanning.load(Ordering::Acquire)
    }

    /// Subscribes to the events the adapter emits (for assertions).
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<CentralEvent> {
        self.inner.events.subscribe()
    }

    pub(super) fn in_range_devices(&self) -> Vec<Peripheral> {
        self.inner
            .devices
            .lock()
            .unwrap()
            .iter()
            .filter(|d| d.in_range.load(Ordering::Acquire))
            .cloned()
            .map(Peripheral::new)
            .collect()
    }

    /// Every device in the world, in range or not and known or not: the
    /// system wide record a retrieval by identifier draws from.
    pub(super) fn all_devices(&self) -> Vec<Peripheral> {
        self.inner
            .devices
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .map(Peripheral::new)
            .collect()
    }

    pub(super) fn device_by_id(&self, id: &PeripheralId) -> Option<Peripheral> {
        self.inner
            .devices
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.id == *id)
            .cloned()
            .map(Peripheral::new)
    }
}

impl WorldInner {
    pub(super) fn emit(&self, event: CentralEvent) {
        // No receivers is fine: nobody asked for events yet.
        let _ = self.events.send(event);
    }
}

/// Description of a mock device: its advertisement and its GATT database.
#[derive(Clone, Debug)]
pub struct DeviceSpec {
    pub address: BDAddr,
    pub name: Option<String>,
    pub services: Vec<ServiceSpec>,
    /// Service UUIDs in the advertisement. Defaults to every service in `services`.
    pub advertised_services: Option<Vec<Uuid>>,
    pub manufacturer_data: HashMap<u16, Vec<u8>>,
    pub service_data: HashMap<Uuid, Vec<u8>>,
    pub rssi: Option<i16>,
    pub tx_power_level: Option<i16>,
    pub mtu: u16,
}

impl DeviceSpec {
    /// A device with the given address, no name and no services.
    pub fn new(address: impl Into<BDAddr>) -> Self {
        Self {
            address: address.into(),
            name: None,
            services: vec![],
            advertised_services: None,
            manufacturer_data: HashMap::new(),
            service_data: HashMap::new(),
            rssi: Some(-60),
            tx_power_level: None,
            mtu: 247,
        }
    }

    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    #[must_use]
    pub fn service(mut self, service: ServiceSpec) -> Self {
        self.services.push(service);
        self
    }

    /// Overrides the advertised service list (by default all services are advertised).
    #[must_use]
    pub fn advertised_services(mut self, services: Vec<Uuid>) -> Self {
        self.advertised_services = Some(services);
        self
    }

    #[must_use]
    pub fn manufacturer_data(mut self, id: u16, data: Vec<u8>) -> Self {
        self.manufacturer_data.insert(id, data);
        self
    }

    #[must_use]
    pub fn service_data(mut self, service: Uuid, data: Vec<u8>) -> Self {
        self.service_data.insert(service, data);
        self
    }

    #[must_use]
    pub fn rssi(mut self, rssi: Option<i16>) -> Self {
        self.rssi = rssi;
        self
    }

    #[must_use]
    pub fn mtu(mut self, mtu: u16) -> Self {
        self.mtu = mtu;
        self
    }

    fn advertised(&self) -> Vec<Uuid> {
        self.advertised_services
            .clone()
            .unwrap_or_else(|| self.services.iter().map(|s| s.uuid).collect())
    }

    fn gatt(&self) -> BTreeSet<Service> {
        self.services
            .iter()
            .map(|s| Service {
                uuid: s.uuid,
                primary: true,
                characteristics: s
                    .characteristics
                    .iter()
                    .map(|(uuid, properties)| Characteristic {
                        uuid: *uuid,
                        service_uuid: s.uuid,
                        properties: *properties,
                        descriptors: BTreeSet::new(),
                    })
                    .collect(),
            })
            .collect()
    }
}

/// A GATT service of a [`DeviceSpec`].
#[derive(Clone, Debug)]
pub struct ServiceSpec {
    pub uuid: Uuid,
    pub characteristics: Vec<(Uuid, CharPropFlags)>,
}

impl ServiceSpec {
    #[must_use]
    pub fn new(uuid: Uuid) -> Self {
        Self {
            uuid,
            characteristics: vec![],
        }
    }

    #[must_use]
    pub fn characteristic(mut self, uuid: Uuid, properties: CharPropFlags) -> Self {
        self.characteristics.push((uuid, properties));
        self
    }
}

/// The error a failing mock operation returns. `btleplug::Error` is not `Clone`,
/// so behaviours store this and build the error on every use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailKind {
    NotConnected,
    DeviceNotFound,
    TimedOut,
    NotSupported,
    Runtime(String),
}

impl FailKind {
    #[must_use]
    pub fn to_error(&self) -> btleplug::Error {
        match self {
            FailKind::NotConnected => btleplug::Error::NotConnected,
            FailKind::DeviceNotFound => btleplug::Error::DeviceNotFound,
            FailKind::TimedOut => btleplug::Error::TimedOut(Duration::from_secs(0)),
            FailKind::NotSupported => btleplug::Error::NotSupported("mock".to_string()),
            FailKind::Runtime(msg) => btleplug::Error::RuntimeError(msg.clone()),
        }
    }
}

/// How the device answers `connect()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectBehaviour {
    /// Connects and emits `DeviceConnected` right away.
    Ok,
    /// `connect()` takes this long, then behaves like `Ok`.
    OkAfter(Duration),
    /// `connect()` returns this error.
    Fail(FailKind),
    /// `connect()` returns `Ok(())` but the link never comes up and no event
    /// is emitted (a `connectGatt` that never completes). Use
    /// [`DeviceHandle::complete_connect`] to finish it later.
    Hang,
    /// Connects, then drops the link (with event) after the delay.
    OkThenDrop(Duration),
}

/// How the device answers a GATT operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpBehaviour {
    Ok,
    /// Succeeds after the delay.
    Delay(Duration),
    Fail(FailKind),
    /// Never completes.
    Hang,
}

/// A call the handler made on a mock peripheral, for assertions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Connect,
    Disconnect,
    DiscoverServices,
    Read(Uuid),
    Write(Uuid, Vec<u8>, WriteType),
    Subscribe(Uuid),
    Unsubscribe(Uuid),
}

#[derive(Clone, Debug)]
pub(super) struct Behaviour {
    pub connect: ConnectBehaviour,
    /// Consumed before `connect` is used, one entry per `connect()` call.
    pub connect_queue: VecDeque<ConnectBehaviour>,
    pub disconnect: OpBehaviour,
    /// Whether a successful `disconnect()` emits `DeviceDisconnected`.
    pub disconnect_emits_event: bool,
    /// Whether losing the link makes the backend forget the peripheral, the way
    /// CoreBluetooth does. See [`DeviceHandle::set_forget_on_disconnect`].
    pub forget_on_disconnect: bool,
    pub discover: OpBehaviour,
    pub read: OpBehaviour,
    pub write: OpBehaviour,
    pub subscribe: OpBehaviour,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            connect: ConnectBehaviour::Ok,
            connect_queue: VecDeque::new(),
            disconnect: OpBehaviour::Ok,
            disconnect_emits_event: true,
            forget_on_disconnect: false,
            discover: OpBehaviour::Ok,
            read: OpBehaviour::Ok,
            write: OpBehaviour::Ok,
            subscribe: OpBehaviour::Ok,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum NotifEvent {
    Value(ValueNotification),
    /// The link went down, the notification stream ends.
    End,
}

#[derive(Debug)]
pub(super) struct DeviceInner {
    pub(super) id: PeripheralId,
    pub(super) spec: DeviceSpec,
    pub(super) gatt: BTreeSet<Service>,
    events: broadcast::Sender<CentralEvent>,
    pub(super) in_range: AtomicBool,
    /// Whether the backend still has a record of this peripheral. Only a known
    /// peripheral can be connected; see [`DeviceHandle::forget`].
    pub(super) known: AtomicBool,
    pub(super) connected: AtomicBool,
    pub(super) discovered: AtomicBool,
    /// Characteristic values keyed by (service, characteristic)
    pub(super) values: Mutex<HashMap<(Uuid, Uuid), Vec<u8>>>,
    pub(super) subscriptions: Mutex<HashSet<(Uuid, Uuid)>>,
    pub(super) notifications: broadcast::Sender<NotifEvent>,
    pub(super) behaviour: Mutex<Behaviour>,
    pub(super) ops: Mutex<Vec<Op>>,
}

impl DeviceInner {
    fn new(spec: DeviceSpec, events: broadcast::Sender<CentralEvent>) -> Self {
        let (notifications, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            id: peripheral_id(spec.address),
            gatt: spec.gatt(),
            spec,
            events,
            in_range: AtomicBool::new(true),
            known: AtomicBool::new(true),
            connected: AtomicBool::new(false),
            discovered: AtomicBool::new(false),
            values: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashSet::new()),
            notifications,
            behaviour: Mutex::new(Behaviour::default()),
            ops: Mutex::new(vec![]),
        }
    }

    pub(super) fn advertised_services(&self) -> Vec<Uuid> {
        self.spec.advertised()
    }

    pub(super) fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub(super) fn is_in_range(&self) -> bool {
        self.in_range.load(Ordering::Acquire)
    }

    pub(super) fn is_known(&self) -> bool {
        self.known.load(Ordering::Acquire)
    }

    pub(super) fn set_known(&self, known: bool) {
        self.known.store(known, Ordering::Release);
    }

    pub(super) fn record(&self, op: Op) {
        self.ops.lock().unwrap().push(op);
    }

    pub(super) fn emit(&self, event: CentralEvent) {
        let _ = self.events.send(event);
    }

    /// Brings the link up and emits `DeviceConnected`.
    pub(super) fn establish_link(&self) {
        self.connected.store(true, Ordering::Release);
        self.emit(CentralEvent::DeviceConnected(self.id.clone()));
    }

    /// Takes the link down. Returns whether it was up. Subscriptions are
    /// forgotten and the notification stream ends; `DeviceDisconnected` is
    /// emitted only when `emit_event` is set.
    pub(super) fn drop_link(&self, emit_event: bool) -> bool {
        let was_connected = self.connected.swap(false, Ordering::AcqRel);
        if !was_connected {
            return false;
        }
        if self.behaviour.lock().unwrap().forget_on_disconnect {
            self.set_known(false);
        }
        self.discovered.store(false, Ordering::Release);
        self.subscriptions.lock().unwrap().clear();
        let _ = self.notifications.send(NotifEvent::End);
        if emit_event {
            self.emit(CentralEvent::DeviceDisconnected(self.id.clone()));
        }
        true
    }

    pub(super) fn find_characteristic(
        &self,
        service: Uuid,
        charac: Uuid,
    ) -> Option<&Characteristic> {
        self.gatt
            .iter()
            .find(|s| s.uuid == service)
            .and_then(|s| s.characteristics.iter().find(|c| c.uuid == charac))
    }

    /// Delivers a notification if the central subscribed to the characteristic.
    pub(super) fn notify(&self, service: Uuid, charac: Uuid, value: Vec<u8>) -> bool {
        if !self.is_connected()
            || !self
                .subscriptions
                .lock()
                .unwrap()
                .contains(&(service, charac))
        {
            return false;
        }
        let _ = self
            .notifications
            .send(NotifEvent::Value(ValueNotification {
                uuid: charac,
                service_uuid: service,
                value,
            }));
        true
    }
}

/// Control handle of one mock device. Cloning shares the device.
#[derive(Clone, Debug)]
pub struct DeviceHandle(pub(super) Arc<DeviceInner>);

impl DeviceHandle {
    #[must_use]
    pub fn address(&self) -> BDAddr {
        self.0.spec.address
    }

    /// The address formatted the way the handler and the JS API use it.
    #[must_use]
    pub fn address_string(&self) -> String {
        crate::models::fmt_addr(self.0.spec.address)
    }

    #[must_use]
    pub fn id(&self) -> PeripheralId {
        self.0.id.clone()
    }

    /// The btleplug peripheral view of this device.
    #[must_use]
    pub fn peripheral(&self) -> Peripheral {
        Peripheral::new(self.0.clone())
    }

    /// Whether the central currently has a link to this device.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.0.is_connected()
    }

    /// In range devices show up in scans and can be connected. Out of range
    /// ones are gone from `peripherals()` and `connect()` fails. An existing
    /// link is left alone; use [`Self::drop_link`] for that.
    pub fn set_in_range(&self, in_range: bool) {
        self.0.in_range.store(in_range, Ordering::Release);
    }

    #[must_use]
    pub fn is_in_range(&self) -> bool {
        self.0.is_in_range()
    }

    /// The backend forgets its record of the peripheral: it stays in range and
    /// keeps showing up in `peripherals()`, but `connect()` fails until it is
    /// retrieved (`retrieve_peripherals`) or a scan finds it again. This is how
    /// CoreBluetooth treats a peripheral whose link went down.
    pub fn forget(&self) {
        self.0.set_known(false);
    }

    /// Whether the backend has a record of the peripheral, see [`Self::forget`].
    #[must_use]
    pub fn is_known(&self) -> bool {
        self.0.is_known()
    }

    /// Whether losing the link makes the backend [`Self::forget`] the
    /// peripheral (default false). CoreBluetooth does this, so with `true` a
    /// device that was connected once can only be reconnected after a retrieval
    /// or a fresh scan.
    pub fn set_forget_on_disconnect(&self, forget: bool) {
        self.0.behaviour.lock().unwrap().forget_on_disconnect = forget;
    }

    pub fn set_connect_behaviour(&self, behaviour: ConnectBehaviour) {
        self.0.behaviour.lock().unwrap().connect = behaviour;
    }

    /// Queues behaviours used by the next `connect()` calls, before falling
    /// back to the one set with [`Self::set_connect_behaviour`].
    pub fn queue_connect_behaviours(&self, behaviours: impl IntoIterator<Item = ConnectBehaviour>) {
        self.0
            .behaviour
            .lock()
            .unwrap()
            .connect_queue
            .extend(behaviours);
    }

    /// Makes the next `n` `connect()` calls fail with `kind`.
    pub fn fail_next_connects(&self, n: usize, kind: FailKind) {
        self.queue_connect_behaviours(std::iter::repeat(ConnectBehaviour::Fail(kind)).take(n));
    }

    /// Drops queued connect behaviours that were not consumed yet.
    pub fn clear_connect_queue(&self) {
        self.0.behaviour.lock().unwrap().connect_queue.clear();
    }

    pub fn set_disconnect_behaviour(&self, behaviour: OpBehaviour) {
        self.0.behaviour.lock().unwrap().disconnect = behaviour;
    }

    /// Whether `disconnect()` emits `DeviceDisconnected` (default true). With
    /// `false` the handler has to cope with a missing disconnect event.
    pub fn set_disconnect_emits_event(&self, emits: bool) {
        self.0.behaviour.lock().unwrap().disconnect_emits_event = emits;
    }

    pub fn set_discover_behaviour(&self, behaviour: OpBehaviour) {
        self.0.behaviour.lock().unwrap().discover = behaviour;
    }

    pub fn set_read_behaviour(&self, behaviour: OpBehaviour) {
        self.0.behaviour.lock().unwrap().read = behaviour;
    }

    pub fn set_write_behaviour(&self, behaviour: OpBehaviour) {
        self.0.behaviour.lock().unwrap().write = behaviour;
    }

    pub fn set_subscribe_behaviour(&self, behaviour: OpBehaviour) {
        self.0.behaviour.lock().unwrap().subscribe = behaviour;
    }

    /// The link breaks: the device is disconnected, the notification stream
    /// ends and `DeviceDisconnected` is emitted. Returns whether a link existed.
    pub fn drop_link(&self) -> bool {
        self.0.drop_link(true)
    }

    /// Like [`Self::drop_link`] but without the event: the adapter never tells
    /// the handler, which only finds out via `is_connected()`.
    pub fn drop_link_silently(&self) -> bool {
        self.0.drop_link(false)
    }

    /// Brings the link up and emits `DeviceConnected`, e.g. to finish a
    /// [`ConnectBehaviour::Hang`] late or to simulate a connection nobody asked for.
    pub fn complete_connect(&self) {
        self.0.establish_link();
    }

    /// Emits a `DeviceConnected` event for this device without changing its state.
    pub fn emit_connected_event(&self) {
        self.0
            .emit(CentralEvent::DeviceConnected(self.0.id.clone()));
    }

    /// Emits a `DeviceDisconnected` event for this device without changing its state.
    pub fn emit_disconnected_event(&self) {
        self.0
            .emit(CentralEvent::DeviceDisconnected(self.0.id.clone()));
    }

    /// Sends a notification. Returns false (and sends nothing) if the central
    /// is not connected or not subscribed to that characteristic.
    pub fn notify(&self, service: Uuid, charac: Uuid, value: Vec<u8>) -> bool {
        self.0.notify(service, charac, value)
    }

    /// Sets the value a `read` returns.
    pub fn set_value(&self, service: Uuid, charac: Uuid, value: Vec<u8>) {
        self.0
            .values
            .lock()
            .unwrap()
            .insert((service, charac), value);
    }

    /// The last value written by the central (or set with [`Self::set_value`]).
    #[must_use]
    pub fn value(&self, service: Uuid, charac: Uuid) -> Option<Vec<u8>> {
        self.0
            .values
            .lock()
            .unwrap()
            .get(&(service, charac))
            .cloned()
    }

    /// Characteristics (service, characteristic) the central is subscribed to.
    #[must_use]
    pub fn subscriptions(&self) -> HashSet<(Uuid, Uuid)> {
        self.0.subscriptions.lock().unwrap().clone()
    }

    /// Every call the handler made on this device, in order.
    #[must_use]
    pub fn ops(&self) -> Vec<Op> {
        self.0.ops.lock().unwrap().clone()
    }

    /// Number of recorded calls equal to `op`.
    #[must_use]
    pub fn count_ops(&self, op: &Op) -> usize {
        self.0
            .ops
            .lock()
            .unwrap()
            .iter()
            .filter(|o| *o == op)
            .count()
    }

    pub fn clear_ops(&self) {
        self.0.ops.lock().unwrap().clear();
    }
}
