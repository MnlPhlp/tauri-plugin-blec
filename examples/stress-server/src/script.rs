//! Scripted integration tests ("Integration scripts" in ../stress-protocol.md).
//!
//! Each script is a fixed timeline. The engine performs the server side actions at
//! their offsets from `t0` and resolves the server steps; observation based steps
//! wait for a counter condition and fail when their window elapses. The REPORT
//! characteristic exposes the run as JSON.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{bail, Result};
use tokio::{
    task::JoinHandle,
    time::{sleep_until, timeout_at},
};

use crate::control::{log, ServerState};

// -------------------------------------------------------------------------------------------
// Definitions
// -------------------------------------------------------------------------------------------

/// Static description of one script: id, name and its server steps in report order.
pub struct Definition {
    pub id: u8,
    pub name: &'static str,
    pub steps: &'static [&'static str],
}

pub const SCRIPTS: [Definition; 7] = [
    Definition {
        id: 1,
        name: "echo-baseline",
        steps: &["echo_writes_received"],
    },
    Definition {
        id: 2,
        name: "link-loss",
        steps: &["drop", "client_reconnected", "echo_after_reconnect"],
    },
    Definition {
        id: 3,
        name: "disappear",
        steps: &["hide", "show", "client_reconnected", "echo_after_reconnect"],
    },
    Definition {
        id: 4,
        name: "notification-storm",
        steps: &["storm"],
    },
    Definition {
        id: 5,
        name: "slow-and-fail",
        steps: &[
            "slow_on",
            "slow_echo_received",
            "slow_off_fail_armed",
            "fails_consumed",
        ],
    },
    Definition {
        id: 6,
        name: "restart",
        steps: &["down", "up", "client_reconnected", "echo_after_restart"],
    },
    Definition {
        id: 7,
        name: "rapid-reconnect",
        steps: &["connects_seen", "echo_seen"],
    },
];

pub fn definition(id: u8) -> Option<&'static Definition> {
    SCRIPTS.iter().find(|d| d.id == id)
}

// -------------------------------------------------------------------------------------------
// Run state
// -------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Running,
    Done,
    Aborted,
}

impl RunState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Done => "done",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Step {
    pub name: &'static str,
    /// `None` while pending.
    pub ok: Option<bool>,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct ScriptRun {
    pub id: u8,
    pub name: &'static str,
    pub t0: Instant,
    pub finished: Option<Instant>,
    pub state: RunState,
    pub steps: Vec<Step>,
    /// Distinguishes runs so a late resolve from a superseded task is ignored.
    generation: u64,
}

impl ScriptRun {
    fn idle() -> Self {
        Self {
            id: 0,
            name: "",
            t0: Instant::now(),
            finished: None,
            state: RunState::Idle,
            steps: Vec::new(),
            generation: 0,
        }
    }

    fn new(def: &Definition, generation: u64) -> Self {
        Self {
            id: def.id,
            name: def.name,
            t0: Instant::now(),
            finished: None,
            state: RunState::Running,
            steps: def
                .steps
                .iter()
                .map(|name| Step {
                    name,
                    ok: None,
                    detail: "waiting".to_string(),
                })
                .collect(),
            generation,
        }
    }

    fn elapsed_ms(&self) -> u128 {
        match self.state {
            RunState::Idle => 0,
            _ => self
                .finished
                .unwrap_or_else(Instant::now)
                .duration_since(self.t0)
                .as_millis(),
        }
    }
}

// -------------------------------------------------------------------------------------------
// Engine
// -------------------------------------------------------------------------------------------

struct Slot {
    run: ScriptRun,
    task: Option<JoinHandle<()>>,
    next_generation: u64,
}

/// Owns the current (last started) script run and its timeline task.
pub struct Engine {
    slot: Mutex<Slot>,
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            slot: Mutex::new(Slot {
                run: ScriptRun::idle(),
                task: None,
                next_generation: 1,
            }),
        }
    }
}

impl Engine {
    fn lock(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Start script `id`, aborting a running one first.
    pub fn start(&self, state: &Arc<ServerState>, id: u8) -> Result<()> {
        let Some(def) = definition(id) else {
            bail!("unknown script id {id} (valid: 1-7)");
        };
        let mut slot = self.lock();
        Self::abort_locked(&mut slot);
        let generation = slot.next_generation;
        slot.next_generation += 1;
        slot.run = ScriptRun::new(def, generation);
        log("script", format!("start id={id} name={}", def.name));
        let ctx = Ctx {
            state: state.clone(),
            generation,
            t0: slot.run.t0,
        };
        slot.task = Some(tokio::spawn(async move { run_script(ctx, id).await }));
        Ok(())
    }

    /// Abort the running script. Errors if none is running.
    pub fn abort(&self) -> Result<()> {
        let mut slot = self.lock();
        if slot.run.state != RunState::Running {
            bail!("no script running");
        }
        Self::abort_locked(&mut slot);
        Ok(())
    }

    fn abort_locked(slot: &mut Slot) {
        if let Some(task) = slot.task.take() {
            task.abort();
        }
        if slot.run.state == RunState::Running {
            slot.run.state = RunState::Aborted;
            slot.run.finished = Some(Instant::now());
            log("script", "aborted");
        }
    }

    /// Resolve the pending step `name` of run `generation`.
    fn resolve(&self, generation: u64, name: &str, ok: bool, detail: String) {
        let mut slot = self.lock();
        let run = &mut slot.run;
        if run.generation != generation || run.state != RunState::Running {
            return;
        }
        let Some(step) = run
            .steps
            .iter_mut()
            .find(|s| s.name == name && s.ok.is_none())
        else {
            log("error", format!("script step {name} not pending"));
            return;
        };
        step.ok = Some(ok);
        step.detail = detail;
        log("script", format!("step {name} ok={ok} {}", step.detail));
        if run.steps.iter().all(|s| s.ok.is_some()) {
            let pass = run.steps.iter().all(|s| s.ok == Some(true));
            run.state = RunState::Done;
            run.finished = Some(Instant::now());
            log("script", format!("done id={} pass={pass}", run.id));
        }
    }

    /// Snapshot of the current run.
    pub fn current(&self) -> ScriptRun {
        self.lock().run.clone()
    }

    /// REPORT characteristic payload.
    pub fn report_json(&self) -> String {
        let run = self.current();
        let steps: Vec<serde_json::Value> = run
            .steps
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "ok": s.ok,
                    "detail": s.detail,
                })
            })
            .collect();
        serde_json::json!({
            "script": run.id,
            "name": run.name,
            "state": run.state.as_str(),
            "elapsed_ms": u64::try_from(run.elapsed_ms()).unwrap_or(u64::MAX),
            "steps": steps,
        })
        .to_string()
    }
}

// -------------------------------------------------------------------------------------------
// Timelines
// -------------------------------------------------------------------------------------------

/// Everything a timeline task needs.
struct Ctx {
    state: Arc<ServerState>,
    generation: u64,
    t0: Instant,
}

impl Ctx {
    /// Sleep until `t0 + offset`.
    async fn at(&self, offset: Duration) {
        sleep_until((self.t0 + offset).into()).await;
    }

    fn resolve(&self, name: &str, ok: bool, detail: impl Into<String>) {
        self.state
            .script
            .resolve(self.generation, name, ok, detail.into());
    }

    /// Wait until `cond` holds or `deadline` passes. Race free: the watch version is
    /// marked seen before each check, so an event between check and await still wakes us.
    async fn wait_until(&self, deadline: Instant, mut cond: impl FnMut() -> bool) -> bool {
        let mut rx = self.state.subscribe();
        loop {
            rx.borrow_and_update();
            if cond() {
                return true;
            }
            match timeout_at(deadline.into(), rx.changed()).await {
                Ok(Ok(())) => {}
                // Sender gone (never happens) or deadline reached: final check.
                Ok(Err(_)) | Err(_) => return cond(),
            }
        }
    }

    /// Resolve an observation step that needs `counter() - baseline >= needed` within
    /// the window ending at `deadline`. Returns whether it passed.
    async fn wait_count(
        &self,
        step: &str,
        counter: impl Fn() -> u64,
        baseline: u64,
        needed: u64,
        deadline: Instant,
        what: &str,
    ) -> bool {
        let ok = self
            .wait_until(deadline, || counter().saturating_sub(baseline) >= needed)
            .await;
        let seen = counter().saturating_sub(baseline);
        let detail = if ok {
            format!("{seen} {what}")
        } else {
            format!("{seen}/{needed} {what} before window elapsed")
        };
        self.resolve(step, ok, detail);
        ok
    }

    /// `client_reconnected`: a connection observed within 15 s after `since`.
    /// Returns the instant the reconnect was seen, if it passed.
    async fn wait_reconnect(&self, connects_before: u64, since: Instant) -> Option<Instant> {
        let ok = self
            .wait_count(
                "client_reconnected",
                || self.state.connects(),
                connects_before,
                1,
                since + Duration::from_secs(15),
                "connection(s)",
            )
            .await;
        ok.then(Instant::now)
    }

    /// `<step>`: 5 ECHO writes within 10 s after `reconnected_at`; fails
    /// immediately if there was no reconnect.
    async fn wait_echo_after(&self, step: &str, reconnected_at: Option<Instant>) {
        let Some(since) = reconnected_at else {
            self.resolve(step, false, "skipped: client did not reconnect");
            return;
        };
        let baseline = self.state.echo_writes();
        self.wait_count(
            step,
            || self.state.echo_writes(),
            baseline,
            5,
            since + Duration::from_secs(10),
            "echo writes",
        )
        .await;
    }
}

const fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

async fn run_script(ctx: Ctx, id: u8) {
    match id {
        1 => echo_baseline(&ctx).await,
        2 => link_loss(&ctx).await,
        3 => disappear(&ctx).await,
        4 => notification_storm(&ctx).await,
        5 => slow_and_fail(&ctx).await,
        6 => restart(&ctx).await,
        7 => rapid_reconnect(&ctx).await,
        _ => {}
    }
}

/// 1 `echo-baseline`: 40 ECHO writes within 10 s.
async fn echo_baseline(ctx: &Ctx) {
    let st = &ctx.state;
    let baseline = st.echo_writes();
    ctx.wait_count(
        "echo_writes_received",
        || st.echo_writes(),
        baseline,
        40,
        ctx.t0 + secs(10),
        "echo writes",
    )
    .await;
}

/// 2 `link-loss`: drop at 2 s, reconnect within 15 s, 5 echoes within 10 s after that.
async fn link_loss(ctx: &Ctx) {
    let st = &ctx.state;
    ctx.at(secs(2)).await;
    let connects_before = st.connects();
    match st.drop_link(None).await {
        Ok(addr) => ctx.resolve("drop", true, format!("disconnected {addr}")),
        Err(err) => ctx.resolve("drop", false, format!("{err:#}")),
    }
    let dropped_at = Instant::now();
    let reconnected = ctx.wait_reconnect(connects_before, dropped_at).await;
    ctx.wait_echo_after("echo_after_reconnect", reconnected)
        .await;
}

/// 3 `disappear`: hide 6 s at 1 s and drop, show at 7 s, reconnect, echoes.
async fn disappear(ctx: &Ctx) {
    let st = &ctx.state;
    ctx.at(secs(1)).await;
    let connects_before = st.connects();
    let hide = st.hide(6).await;
    let drop = st.drop_link(None).await;
    match (hide, drop) {
        (Ok(()), Ok(addr)) => ctx.resolve("hide", true, format!("hidden 6s, disconnected {addr}")),
        (Err(err), _) => ctx.resolve("hide", false, format!("hide failed: {err:#}")),
        (Ok(()), Err(err)) => {
            ctx.resolve("hide", false, format!("hidden 6s, drop failed: {err:#}"));
        }
    }

    ctx.at(secs(7)).await;
    // The hide timer re-advertises on its own at 7 s; show() is idempotent.
    let shown = match st.show().await {
        Ok(()) => {
            ctx.resolve("show", true, "advertising");
            true
        }
        Err(err) => {
            ctx.resolve("show", false, format!("{err:#}"));
            false
        }
    };
    let shown_at = Instant::now();
    if !shown {
        // Nobody can connect to a hidden peripheral; resolve the rest right away.
        ctx.resolve("client_reconnected", false, "skipped: not advertising");
        ctx.wait_echo_after("echo_after_reconnect", None).await;
        return;
    }
    let reconnected = ctx.wait_reconnect(connects_before, shown_at).await;
    ctx.wait_echo_after("echo_after_reconnect", reconnected)
        .await;
}

/// 4 `notification-storm`: 500 COUNTER notifications at 5 ms starting at 1 s.
async fn notification_storm(ctx: &Ctx) {
    let st = &ctx.state;
    ctx.at(secs(1)).await;
    match st.storm(500, Duration::from_millis(5)).await {
        Err(err) => ctx.resolve("storm", false, err.to_string()),
        Ok(rx) => match rx.await {
            Ok(res) => {
                let ok = res.sent == 500 && res.error.is_none();
                let detail = match res.error {
                    Some(err) => format!("sent={} error={err}", res.sent),
                    None => format!("sent={}", res.sent),
                };
                ctx.resolve("storm", ok, detail);
            }
            Err(_) => ctx.resolve("storm", false, "COUNTER session ended during storm"),
        },
    }
}

/// 5 `slow-and-fail`: slow 1500 ms at 0 s, 3 slow echoes before 8 s, at 8 s slow off +
/// 2 failures armed, both consumed and a LARGE read ok by 15 s.
async fn slow_and_fail(ctx: &Ctx) {
    let st = &ctx.state;
    st.set_slow(1500);
    ctx.resolve("slow_on", true, "1500ms");

    let echo_before = st.echo_writes();
    ctx.wait_count(
        "slow_echo_received",
        || st.echo_writes(),
        echo_before,
        3,
        ctx.t0 + secs(8),
        "echo writes while slow",
    )
    .await;

    ctx.at(secs(8)).await;
    st.set_slow(0);
    st.fail_next(2);
    ctx.resolve("slow_off_fail_armed", true, "slow=0 fail=2");

    let fails_before = st.fails_consumed();
    let large_before = st.large_reads_ok();
    let ok = ctx
        .wait_until(ctx.t0 + secs(15), || {
            st.fails_consumed() - fails_before >= 2 && st.large_reads_ok() - large_before >= 1
        })
        .await;
    ctx.resolve(
        "fails_consumed",
        ok,
        format!(
            "fails_consumed={} large_reads_ok={}",
            st.fails_consumed() - fails_before,
            st.large_reads_ok() - large_before
        ),
    );
}

/// 6 `restart`: at 1 s drop + unregister for 4 s, up at 5 s, reconnect, echoes.
async fn restart(ctx: &Ctx) {
    let st = &ctx.state;
    ctx.at(secs(1)).await;
    let connects_before = st.connects();
    match st.restart(4).await {
        Ok(Some(addr)) => ctx.resolve("down", true, format!("dropped {addr}, unregistered 4s")),
        Ok(None) => ctx.resolve("down", false, "unregistered 4s but no link to drop"),
        Err(err) => ctx.resolve("down", false, format!("{err:#}")),
    }

    ctx.at(secs(5)).await;
    // restart() re-registers on its own timer; allow a few seconds for BlueZ.
    let up = ctx.wait_until(ctx.t0 + secs(10), || st.is_up()).await;
    ctx.resolve(
        "up",
        up,
        if up {
            "application and advertising registered"
        } else {
            "not registered within 5s"
        },
    );
    let up_at = Instant::now();
    if !up {
        ctx.resolve("client_reconnected", false, "skipped: server not up");
        ctx.wait_echo_after("echo_after_restart", None).await;
        return;
    }
    let reconnected = ctx.wait_reconnect(connects_before, up_at).await;
    ctx.wait_echo_after("echo_after_restart", reconnected).await;
}

/// 7 `rapid-reconnect`: at least 10 connections and 10 ECHO writes within 60 s.
async fn rapid_reconnect(ctx: &Ctx) {
    let st = &ctx.state;
    let connects_before = st.connects();
    let echo_before = st.echo_writes();
    let deadline = ctx.t0 + secs(60);
    tokio::join!(
        ctx.wait_count(
            "connects_seen",
            || st.connects(),
            connects_before,
            10,
            deadline,
            "connection(s)",
        ),
        ctx.wait_count(
            "echo_seen",
            || st.echo_writes(),
            echo_before,
            10,
            deadline,
            "echo writes",
        ),
    );
}
