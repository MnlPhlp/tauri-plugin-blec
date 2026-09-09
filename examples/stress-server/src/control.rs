//! Shared server state and the chaos actions.
//!
//! Everything that can disturb the connection (drop, hide, storm, slow, fail, restart)
//! lives here so the REPL, the script engine and CONTROL characteristic writes all go
//! through exactly the same code path. The state also keeps monotonic observation
//! counters (ECHO writes, connections, LARGE reads, consumed failures) and a `watch`
//! channel that is bumped on every observable event so the script engine can await
//! conditions without polling.

use std::{
    fmt::Display,
    sync::{
        atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use bluer::{
    adv::{Advertisement, AdvertisementHandle},
    gatt::local::{ApplicationHandle, CharacteristicNotifier},
    Adapter, AdapterEvent, Address, DeviceEvent, DeviceProperty,
};
use futures::StreamExt;
use tokio::{
    sync::{mpsc, oneshot, watch, Mutex},
    time::sleep,
};

use crate::{gatt, script::Engine};

/// Manufacturer id used in the advertisement (see ../stress-protocol.md).
pub const MANUFACTURER_ID: u16 = 0xB1EC;
/// Manufacturer data payload.
pub const MANUFACTURER_DATA: [u8; 1] = [0x01];

/// Milliseconds since the unix epoch.
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Emit one protocol log line: `<unix_ms> | <event> | <detail>`.
pub fn log(event: &str, detail: impl Display) {
    println!("{} | {} | {}", now_ms(), event, detail);
}

/// Static configuration coming from the CLI.
#[derive(Clone, Debug)]
pub struct Config {
    pub name: String,
    pub counter_interval: Duration,
    pub large_size: usize,
}

/// Outcome of a storm, reported by the COUNTER session task.
#[derive(Clone, Debug)]
pub struct StormResult {
    /// Notifications actually sent.
    pub sent: u32,
    /// First notify error, if any.
    pub error: Option<String>,
}

/// A request for the running COUNTER notify session to send extra notifications.
#[derive(Debug)]
pub struct StormReq {
    pub count: u32,
    pub interval: Duration,
    /// Completed with the outcome once the storm finished (or broke off).
    pub done: oneshot::Sender<StormResult>,
}

/// The two `BlueZ` registrations we own. Dropping a handle unregisters it.
#[derive(Default)]
struct Handles {
    adv: Option<AdvertisementHandle>,
    app: Option<ApplicationHandle>,
}

/// Shared, thread safe server state.
pub struct ServerState {
    pub adapter: Adapter,
    pub config: Config,
    pub script: Engine,
    started: Instant,

    handles: Mutex<Handles>,
    hidden: AtomicBool,
    app_up: AtomicBool,
    restarting: AtomicBool,
    /// Bumped on every hide/restart so a stale "un-hide" timer does not re-advertise
    /// while a newer, longer hide is still in progress.
    hide_gen: AtomicU64,

    central: Mutex<Option<Address>>,
    connects: AtomicU32,
    drops: AtomicU32,

    slow_ms: AtomicU16,
    fail_left: AtomicU8,
    echo_seq: AtomicU32,

    // Monotonic observation counters for the script engine.
    echo_writes: AtomicU64,
    large_reads_ok: AtomicU64,
    fails_consumed: AtomicU64,
    /// Version counter bumped on every observable event (see [`Self::subscribe`]).
    events: watch::Sender<u64>,

    /// Active ECHO notify session (session id, notifier).
    /// Held for the whole ECHO write handler: bluer runs every incoming write as
    /// its own task, and two write-without-response commands that arrive back
    /// to back would otherwise interleave at the awaits and swap their seq numbers.
    /// Only sufficient together with the current-thread runtime (see `main.rs`),
    /// which polls the spawned tasks in spawn order.
    pub echo_lock: Mutex<()>,
    echo_notifier: Mutex<Option<(u64, CharacteristicNotifier)>>,
    /// Channel into the active COUNTER notify session (session id, sender).
    counter_tx: Mutex<Option<(u64, mpsc::UnboundedSender<StormReq>)>>,
    session_ids: AtomicU64,
}

impl ServerState {
    pub fn new(adapter: Adapter, config: Config) -> Arc<Self> {
        Arc::new(Self {
            adapter,
            config,
            script: Engine::default(),
            started: Instant::now(),
            handles: Mutex::new(Handles::default()),
            hidden: AtomicBool::new(true),
            app_up: AtomicBool::new(false),
            restarting: AtomicBool::new(false),
            hide_gen: AtomicU64::new(0),
            central: Mutex::new(None),
            connects: AtomicU32::new(0),
            drops: AtomicU32::new(0),
            slow_ms: AtomicU16::new(0),
            fail_left: AtomicU8::new(0),
            echo_seq: AtomicU32::new(0),
            echo_writes: AtomicU64::new(0),
            large_reads_ok: AtomicU64::new(0),
            fails_consumed: AtomicU64::new(0),
            events: watch::channel(0).0,
            echo_lock: Mutex::new(()),
            echo_notifier: Mutex::new(None),
            counter_tx: Mutex::new(None),
            session_ids: AtomicU64::new(1),
        })
    }

    // ------------------------------------------------------------------------------------
    // Observation plumbing
    // ------------------------------------------------------------------------------------

    /// Wake everybody waiting in [`Self::subscribe`].
    fn bump(&self) {
        self.events.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Receiver that changes whenever a counter or the registration state changed.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.events.subscribe()
    }

    /// Total ECHO writes accepted (not failed) since start.
    pub fn echo_writes(&self) -> u64 {
        self.echo_writes.load(Ordering::SeqCst)
    }

    /// Total central connections observed since start.
    pub fn connects(&self) -> u64 {
        u64::from(self.connects.load(Ordering::SeqCst))
    }

    /// Total successful LARGE reads (offset 0) since start.
    pub fn large_reads_ok(&self) -> u64 {
        self.large_reads_ok.load(Ordering::SeqCst)
    }

    /// Total armed failures that were consumed by a request.
    pub fn fails_consumed(&self) -> u64 {
        self.fails_consumed.load(Ordering::SeqCst)
    }

    /// True when both the GATT application and the advertisement are registered.
    pub fn is_up(&self) -> bool {
        self.app_up.load(Ordering::SeqCst) && !self.hidden.load(Ordering::SeqCst)
    }

    pub fn record_echo_write(&self) {
        self.echo_writes.fetch_add(1, Ordering::SeqCst);
        self.bump();
    }

    pub fn record_large_read_ok(&self) {
        self.large_reads_ok.fetch_add(1, Ordering::SeqCst);
        self.bump();
    }

    // ------------------------------------------------------------------------------------
    // Registration (GATT application + advertisement)
    // ------------------------------------------------------------------------------------

    fn advertisement(&self) -> Advertisement {
        Advertisement {
            service_uuids: [gatt::SERVICE_UUID].into_iter().collect(),
            manufacturer_data: [(MANUFACTURER_ID, MANUFACTURER_DATA.to_vec())]
                .into_iter()
                .collect(),
            discoverable: Some(true),
            local_name: Some(self.config.name.clone()),
            ..Default::default()
        }
    }

    async fn register_adv(&self) -> Result<AdvertisementHandle> {
        let handle = self
            .adapter
            .advertise(self.advertisement())
            .await
            .context("registering advertisement")?;
        self.hidden.store(false, Ordering::SeqCst);
        self.bump();
        log("advertising", &self.config.name);
        Ok(handle)
    }

    async fn register_app(self: &Arc<Self>) -> Result<ApplicationHandle> {
        let handle = self
            .adapter
            .serve_gatt_application(gatt::build_app(self.clone()))
            .await
            .context("registering GATT application")?;
        self.app_up.store(true, Ordering::SeqCst);
        self.bump();
        Ok(handle)
    }

    /// Register the GATT application and start advertising.
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        let mut handles = self.handles.lock().await;
        if handles.app.is_none() {
            handles.app = Some(self.register_app().await?);
        }
        if handles.adv.is_none() {
            handles.adv = Some(self.register_adv().await?);
        }
        Ok(())
    }

    /// Unregister everything (used on quit).
    pub async fn shutdown(&self) {
        let mut handles = self.handles.lock().await;
        handles.app = None;
        handles.adv = None;
        self.hidden.store(true, Ordering::SeqCst);
        self.app_up.store(false, Ordering::SeqCst);
        self.bump();
    }

    // ------------------------------------------------------------------------------------
    // Central tracking
    // ------------------------------------------------------------------------------------

    /// Currently connected central, if known.
    pub async fn central(&self) -> Option<Address> {
        *self.central.lock().await
    }

    /// Called when a central is (or is discovered to be) connected. Idempotent per address.
    pub async fn on_connected(&self, addr: Address) {
        let mut central = self.central.lock().await;
        if *central == Some(addr) {
            return;
        }
        *central = Some(addr);
        self.connects.fetch_add(1, Ordering::SeqCst);
        // ECHO seq starts at 0 for every new connection.
        self.echo_seq.store(0, Ordering::SeqCst);
        log("central_connected", addr);
        self.bump();
    }

    /// Called when a device disconnects. Only acts if it is the tracked central.
    pub async fn on_disconnected(&self, addr: Address) {
        let mut central = self.central.lock().await;
        if *central != Some(addr) {
            return;
        }
        *central = None;
        log("central_disconnected", addr);
        self.bump();
    }

    /// GATT callbacks call this with `req.device_address` so we learn the central
    /// even if the D-Bus device events are late.
    pub async fn note_request_from(&self, addr: Address) {
        self.on_connected(addr).await;
    }

    /// Re-checks the tracked central against `BlueZ` and forgets it when the link
    /// is gone. The `Connected` property change does not always reach the device
    /// watcher, so this runs when a notify session ends (`BlueZ` ends them on
    /// disconnect) and periodically from [`poll_central_liveness`].
    pub async fn verify_central_connected(&self) {
        let Some(addr) = self.central().await else {
            return;
        };
        let connected = match self.adapter.device(addr) {
            Ok(device) => device.is_connected().await.unwrap_or(false),
            Err(_) => false,
        };
        if !connected {
            self.on_disconnected(addr).await;
        }
    }

    // ------------------------------------------------------------------------------------
    // Per request helpers used by gatt.rs
    // ------------------------------------------------------------------------------------

    pub fn next_session_id(&self) -> u64 {
        self.session_ids.fetch_add(1, Ordering::SeqCst)
    }

    pub fn next_echo_seq(&self) -> u32 {
        self.echo_seq.fetch_add(1, Ordering::SeqCst)
    }

    /// Sleep for the configured "slow" delay, if any.
    pub async fn slow_delay(&self) {
        let ms = self.slow_ms.load(Ordering::SeqCst);
        if ms > 0 {
            sleep(Duration::from_millis(u64::from(ms))).await;
        }
    }

    /// Consume one pending failure. Returns true if the current request must fail.
    pub fn take_fail(&self) -> bool {
        let consumed = self
            .fail_left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if consumed {
            self.fails_consumed.fetch_add(1, Ordering::SeqCst);
            self.bump();
        }
        consumed
    }

    pub async fn set_echo_notifier(&self, session: Option<(u64, CharacteristicNotifier)>) {
        *self.echo_notifier.lock().await = session;
    }

    /// Clear the ECHO notifier only if it still belongs to `session_id`.
    pub async fn clear_echo_notifier(&self, session_id: u64) {
        let mut slot = self.echo_notifier.lock().await;
        if matches!(&*slot, Some((id, _)) if *id == session_id) {
            *slot = None;
        }
    }

    /// Send an ECHO notification if a client is subscribed. Returns whether it was sent.
    pub async fn echo_notify(&self, data: Vec<u8>) -> bool {
        let mut slot = self.echo_notifier.lock().await;
        let Some((_, notifier)) = slot.as_mut() else {
            return false;
        };
        match notifier.notify(data).await {
            Ok(()) => true,
            Err(err) => {
                log("error", format!("echo notify failed: {err}"));
                *slot = None;
                false
            }
        }
    }

    pub async fn set_counter_tx(&self, session: Option<(u64, mpsc::UnboundedSender<StormReq>)>) {
        *self.counter_tx.lock().await = session;
    }

    pub async fn clear_counter_tx(&self, session_id: u64) {
        let mut slot = self.counter_tx.lock().await;
        if matches!(&*slot, Some((id, _)) if *id == session_id) {
            *slot = None;
        }
    }

    // ------------------------------------------------------------------------------------
    // Chaos actions
    // ------------------------------------------------------------------------------------

    /// Terminate the link to `addr` (or to the tracked central when `None`).
    /// Returns the address that was disconnected.
    pub async fn drop_link(&self, addr: Option<Address>) -> Result<Address> {
        let addr = match addr {
            Some(a) => a,
            None => self
                .central()
                .await
                .ok_or_else(|| anyhow!("no central connected"))?,
        };
        let device = self.adapter.device(addr)?;
        device
            .disconnect()
            .await
            .with_context(|| format!("disconnecting {addr}"))?;
        self.drops.fetch_add(1, Ordering::SeqCst);
        log("drop", addr);
        // BlueZ replies to Disconnect once the link is down; mark it right away so a
        // quick reconnect is counted even if the D-Bus property change arrives late.
        self.on_disconnected(addr).await;
        Ok(addr)
    }

    /// Stop advertising for `secs` seconds, then advertise again.
    pub async fn hide(self: &Arc<Self>, secs: u64) -> Result<()> {
        if self.restarting.load(Ordering::SeqCst) {
            bail!("restart in progress");
        }
        let gen = self.hide_gen.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut handles = self.handles.lock().await;
            handles.adv = None;
        }
        self.hidden.store(true, Ordering::SeqCst);
        self.bump();
        log("hidden", format!("{secs}s"));

        let state = self.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(secs)).await;
            if state.hide_gen.load(Ordering::SeqCst) != gen {
                // A newer hide/restart superseded us.
                return;
            }
            if let Err(err) = state.show().await {
                log(
                    "error",
                    format!("re-advertising after hide failed: {err:#}"),
                );
            }
        });
        Ok(())
    }

    /// Start advertising (no-op if already advertising).
    pub async fn show(&self) -> Result<()> {
        if self.restarting.load(Ordering::SeqCst) {
            bail!("restart in progress");
        }
        let mut handles = self.handles.lock().await;
        if handles.adv.is_some() {
            return Ok(());
        }
        handles.adv = Some(self.register_adv().await?);
        Ok(())
    }

    /// Ask the running COUNTER session to send `count` extra notifications.
    /// The returned receiver completes with the outcome; callers may drop it.
    pub async fn storm(
        &self,
        count: u32,
        interval: Duration,
    ) -> Result<oneshot::Receiver<StormResult>> {
        let slot = self.counter_tx.lock().await;
        let Some((_, tx)) = slot.as_ref() else {
            bail!("no COUNTER subscriber");
        };
        let (done, rx) = oneshot::channel();
        tx.send(StormReq {
            count,
            interval,
            done,
        })
        .map_err(|_| anyhow!("no COUNTER subscriber"))?;
        log(
            "storm",
            format!(
                "requested count={count} interval={}ms",
                interval.as_millis()
            ),
        );
        Ok(rx)
    }

    /// Delay every read/write/echo response by `ms` (0 = off).
    pub fn set_slow(&self, ms: u16) {
        self.slow_ms.store(ms, Ordering::SeqCst);
        log("slow", format!("{ms}ms"));
    }

    /// Fail the next `count` read/write requests with an ATT error.
    pub fn fail_next(&self, count: u8) {
        self.fail_left.store(count, Ordering::SeqCst);
        log("fail", format!("armed count={count}"));
    }

    /// Drop the link, unregister the GATT application and advertisement, and
    /// re-register after `secs`. Returns the address whose link was dropped
    /// (`None` if no central was connected).
    pub async fn restart(self: &Arc<Self>, secs: u64) -> Result<Option<Address>> {
        if self
            .restarting
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            bail!("restart already in progress");
        }
        let dropped = match self.drop_link(None).await {
            Ok(addr) => Some(addr),
            Err(err) => {
                log("info", format!("restart: link not dropped: {err:#}"));
                None
            }
        };
        // Invalidate pending hide timers.
        self.hide_gen.fetch_add(1, Ordering::SeqCst);
        {
            let mut handles = self.handles.lock().await;
            handles.app = None;
            handles.adv = None;
        }
        self.hidden.store(true, Ordering::SeqCst);
        self.app_up.store(false, Ordering::SeqCst);
        // BlueZ ends notify sessions when the app goes away; forget ours eagerly.
        self.set_echo_notifier(None).await;
        self.set_counter_tx(None).await;
        self.bump();
        log("restart", format!("down {secs}s"));

        let state = self.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(secs)).await;
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                let result = async {
                    let mut handles = state.handles.lock().await;
                    if handles.app.is_none() {
                        handles.app = Some(state.register_app().await?);
                    }
                    if handles.adv.is_none() {
                        handles.adv = Some(state.register_adv().await?);
                    }
                    anyhow::Ok(())
                }
                .await;
                match result {
                    Ok(()) => {
                        log("restart", "up");
                        break;
                    }
                    Err(err) if attempt < 5 => {
                        log(
                            "error",
                            format!("restart attempt {attempt} failed: {err:#}"),
                        );
                        sleep(Duration::from_secs(1)).await;
                    }
                    Err(err) => {
                        log("error", format!("restart gave up: {err:#}"));
                        break;
                    }
                }
            }
            state.restarting.store(false, Ordering::SeqCst);
            state.bump();
        });
        Ok(dropped)
    }

    /// STATUS characteristic payload.
    pub fn status_json(&self) -> String {
        serde_json::json!({
            "uptime_s": self.started.elapsed().as_secs(),
            "connects": self.connects.load(Ordering::SeqCst),
            "drops": self.drops.load(Ordering::SeqCst),
            "slow_ms": self.slow_ms.load(Ordering::SeqCst),
            "hidden": self.hidden.load(Ordering::SeqCst),
            "fail_ops_left": self.fail_left.load(Ordering::SeqCst),
            "echo_seq": self.echo_seq.load(Ordering::SeqCst),
        })
        .to_string()
    }
}

// ----------------------------------------------------------------------------------------
// Connection tracking via BlueZ device events
// ----------------------------------------------------------------------------------------

/// Watch the adapter for device objects and follow their `Connected` property so
/// `central_connected` / `central_disconnected` are logged even without GATT traffic.
///
/// Uses `Adapter::events()` (object added/removed) rather than `discover_devices()`
/// because the latter starts an active scan, which we do not want on a peripheral.
pub async fn watch_devices(state: Arc<ServerState>) -> Result<()> {
    let adapter = state.adapter.clone();
    let mut events = adapter
        .events()
        .await
        .context("subscribing to adapter events")?;

    // Devices BlueZ already knows about (cached or currently connected).
    for addr in adapter.device_addresses().await.unwrap_or_default() {
        spawn_device_watch(state.clone(), addr);
    }

    while let Some(event) = events.next().await {
        match event {
            AdapterEvent::DeviceAdded(addr) => spawn_device_watch(state.clone(), addr),
            AdapterEvent::DeviceRemoved(addr) => state.on_disconnected(addr).await,
            AdapterEvent::PropertyChanged(_) => {}
        }
    }
    Ok(())
}

/// Fallback for missed `Connected` events: checks the tracked central's link
/// state every 250 ms.
pub async fn poll_central_liveness(state: Arc<ServerState>) {
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    loop {
        ticker.tick().await;
        state.verify_central_connected().await;
    }
}

fn spawn_device_watch(state: Arc<ServerState>, addr: Address) {
    tokio::spawn(async move {
        let Ok(device) = state.adapter.device(addr) else {
            return;
        };
        let Ok(mut events) = device.events().await else {
            return;
        };
        // The Connected=true change may have happened before we subscribed.
        if device.is_connected().await.unwrap_or(false) {
            state.on_connected(addr).await;
        }
        while let Some(event) = events.next().await {
            match event {
                DeviceEvent::PropertyChanged(DeviceProperty::Connected(true)) => {
                    state.on_connected(addr).await;
                }
                DeviceEvent::PropertyChanged(DeviceProperty::Connected(false)) => {
                    state.on_disconnected(addr).await;
                }
                DeviceEvent::PropertyChanged(_) => {}
            }
        }
        // Stream ended: device object removed.
        state.on_disconnected(addr).await;
    });
}
