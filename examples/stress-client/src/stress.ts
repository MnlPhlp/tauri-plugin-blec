import {
  connect,
  disconnect,
  getConnectionUpdates,
  getMtu,
  read,
  send,
  setTimeouts,
  startScan,
  stopScan,
  subscribe,
  type BleDevice,
  type Timeouts,
} from "@mnlphlp/plugin-blec";
import { reactive, ref } from "vue";
import { errStr, OpError, StopError } from "./errors";
import { log } from "./log";
import {
  bytesEqual,
  CONTROL_UUID,
  COUNTER_UUID,
  CTRL_SCRIPT_ABORT,
  decodeUtf8,
  ECHO_UUID,
  hex,
  LARGE_UUID,
  makePayload,
  readU32le,
  REPORT_UUID,
  SCRIPT_ECHO_SIZE,
  SERVICE_UUID,
  STATUS_UUID,
  type Report,
  type ServerStatus,
} from "./protocol";
import {
  RUN_ALL_ORDER,
  runScript,
  SCRIPTS,
  type ScriptResult,
  type ScriptSelection,
} from "./scripts";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

export type Scenario = "integration" | "hold";

export const SCENARIOS: { id: Scenario; label: string }[] = [
  { id: "integration", label: "integration (scripted tests)" },
  { id: "hold", label: "hold (soak: stay connected, COUNTER)" },
];

export type StressConfig = {
  /** Substring of the advertised local name that identifies the target. */
  nameFilter: string;
  /** Which integration script(s) to run. */
  script: ScriptSelection;
  /** GATT timeouts, applied with setTimeouts() before the scenario starts. */
  timeouts: Required<Timeouts>;
  /** Wait between reconnect attempts. */
  reconnectBackoffMs: number;
  /** Duration of a single scan while searching for the target. */
  scanTimeoutMs: number;
};

export type Counters = {
  scans: number;
  connectOk: number;
  connectFail: number;
  disconnectsExpected: number;
  disconnectsUnexpected: number;
  disconnectCallbacks: number;
  notifications: number;
  echoMismatches: number;
  seqGaps: number;
  opTimeouts: number;
  opErrors: number;
  iterations: number;
};

export type EchoResult = { ok: boolean; rtt: number; detail: string };

export type EchoSeries = {
  okCount: number;
  results: EchoResult[];
  minRtt: number;
  maxRtt: number;
  detail: string;
};

export function defaultConfig(): StressConfig {
  return {
    nameFilter: "blec_stress",
    script: "all",
    timeouts: {
      connect: 20000,
      discoverServices: 15000,
      read: 10000,
      write: 10000,
      subscribe: 10000,
      disconnect: 10000,
    },
    reconnectBackoffMs: 1000,
    scanTimeoutMs: 10000,
  };
}

function emptyCounters(): Counters {
  return {
    scans: 0,
    connectOk: 0,
    connectFail: 0,
    disconnectsExpected: 0,
    disconnectsUnexpected: 0,
    disconnectCallbacks: 0,
    notifications: 0,
    echoMismatches: 0,
    seqGaps: 0,
    opTimeouts: 0,
    opErrors: 0,
    iterations: 0,
  };
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

export { StopError, OpError, errStr };

function isTimeoutError(msg: string): boolean {
  return msg.includes("timeout") || msg.includes("Timeout");
}

function isUnknownPeripheral(msg: string): boolean {
  return msg.includes("no peripheral with id");
}

/** Simple async FIFO with timeout, used to hand notifications to awaiting code. */
export class FrameQueue {
  private items: number[][] = [];
  private waiters: { resolve: (v: number[]) => void; reject: (e: Error) => void; timer: number }[] =
    [];

  push(item: number[]) {
    const w = this.waiters.shift();
    if (w) {
      clearTimeout(w.timer);
      w.resolve(item);
    } else {
      this.items.push(item);
    }
  }

  clear() {
    this.items = [];
    for (const w of this.waiters) {
      clearTimeout(w.timer);
      w.reject(new Error("queue cleared"));
    }
    this.waiters = [];
  }

  next(timeoutMs: number): Promise<number[]> {
    const item = this.items.shift();
    if (item) return Promise.resolve(item);
    return new Promise((resolve, reject) => {
      const waiter = {
        resolve,
        reject,
        timer: window.setTimeout(() => {
          this.waiters = this.waiters.filter((w) => w !== waiter);
          reject(new Error(`Timeout waiting for notification (${timeoutMs} ms)`));
        }, timeoutMs),
      };
      this.waiters.push(waiter);
    });
  }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/**
 * Owns the plugin interaction: instrumented calls, scanning, connection
 * management, the echo/counter data paths and the two scenarios. The
 * integration scripts in scripts.ts drive it through its public methods.
 */
export class StressRunner {
  readonly counters = reactive<Counters>(emptyCounters());
  readonly running = ref(false);
  readonly scenario = ref<Scenario | null>(null);
  /** Last value reported through getConnectionUpdates(). */
  readonly connected = ref(false);
  readonly startedAt = ref(0);
  /** Results of the integration run, one entry per script in run order. */
  readonly results = reactive<ScriptResult[]>([]);

  private _cfg: StressConfig = defaultConfig();
  private stopRequested = false;
  private wakers = new Set<() => void>();
  private initialized = false;
  private scanning = false;

  private target: BleDevice | null = null;
  /** True while we asked for the current/next disconnect ourselves. */
  private expectDisconnect = true;
  /** Disconnect callbacks received for the current connection. */
  private _connCallbacks = 0;
  private disconnectWaiters: (() => void)[] = [];

  private subscribedEcho = false;
  private subscribedCounter = false;

  private echoQueue = new FrameQueue();
  private echoSeqExpected = 0;
  private counterSeqExpected: number | null = null;
  /** COUNTER notifications received (all connections). */
  counterNotifications = 0;
  /** COUNTER sequence gaps (all connections). */
  counterSeqGaps = 0;

  get cfg(): StressConfig {
    return this._cfg;
  }

  /** Disconnect callbacks received for the current connection. */
  get connCallbacks(): number {
    return this._connCallbacks;
  }

  get targetAddress(): string | null {
    return this.target?.address ?? null;
  }

  /** Register the connection-state listener. Safe to call more than once. */
  async init() {
    if (this.initialized) return;
    this.initialized = true;
    try {
      await getConnectionUpdates((state) => {
        this.connected.value = state;
      });
    } catch (e) {
      log.error(`getConnectionUpdates failed: ${errStr(e)}`);
    }
  }

  // ---- lifecycle ----------------------------------------------------------

  async start(scenario: Scenario, config: StressConfig) {
    if (this.running.value) {
      log.warn("start ignored: a scenario is already running");
      return;
    }
    await this.init();
    this._cfg = { ...config, timeouts: { ...config.timeouts } };
    this.stopRequested = false;
    this.running.value = true;
    this.scenario.value = scenario;
    this.startedAt.value = Date.now();
    this.target = null;
    this.expectDisconnect = true;
    this.subscribedEcho = false;
    this.subscribedCounter = false;
    this.counterNotifications = 0;
    this.counterSeqGaps = 0;
    Object.assign(this.counters, emptyCounters());

    log.info(
      `start ${scenario}: filter="${this._cfg.nameFilter}" script=${this._cfg.script} ` +
        `timeouts=${JSON.stringify(this._cfg.timeouts)} backoff=${this._cfg.reconnectBackoffMs}ms ` +
        `scan=${this._cfg.scanTimeoutMs}ms`
    );

    try {
      await this.op("setTimeouts", () => setTimeouts(this._cfg.timeouts));
      if (scenario === "integration") await this.runIntegration();
      else await this.runHold();
      log.info(`${scenario} finished after ${Date.now() - this.startedAt.value} ms`);
    } catch (e) {
      if (e instanceof StopError) {
        log.info(`${scenario} stopped by user after ${Date.now() - this.startedAt.value} ms`);
      } else {
        log.error(`${scenario} aborted: ${errStr(e)}`);
      }
    } finally {
      await this.cleanup();
      log.info(`counters: ${JSON.stringify(this.counters)}`);
      this.running.value = false;
    }
  }

  /**
   * Cooperative stop: sets the flag, wakes every sleep()/wait and lets the
   * scenario unwind at its next check(). A plugin call that is already in
   * flight is awaited, not interrupted.
   */
  stop() {
    if (!this.running.value || this.stopRequested) return;
    log.info("stop requested");
    this.stopRequested = true;
    for (const w of Array.from(this.wakers)) w();
    this.wakers.clear();
  }

  private async cleanup() {
    if (this.scanning) {
      await stopScan().catch(() => {});
      this.scanning = false;
    }
    if (this.connected.value) {
      if (this.scenario.value === "integration") {
        // leave the server idle even when the run was aborted half-way
        try {
          await send(CONTROL_UUID, [CTRL_SCRIPT_ABORT], "withResponse", SERVICE_UUID);
          log.info("cleanup: CONTROL abort (0x11) sent");
        } catch (e) {
          log.warn(`cleanup: CONTROL abort failed: ${errStr(e)}`);
        }
      }
      log.info("cleanup: disconnecting");
      this.expectDisconnect = true;
      try {
        await disconnect();
      } catch (e) {
        log.warn(`cleanup disconnect failed: ${errStr(e)}`);
      }
    }
    this.echoQueue.clear();
  }

  // ---- cancellation primitives -------------------------------------------

  check() {
    if (this.stopRequested) throw new StopError();
  }

  /** Sleep that returns early (throwing StopError) when stop() is called. */
  sleep(ms: number): Promise<void> {
    this.check();
    return new Promise((resolve, reject) => {
      const timer = window.setTimeout(() => {
        this.wakers.delete(wake);
        resolve();
      }, ms);
      const wake = () => {
        clearTimeout(timer);
        reject(new StopError());
      };
      this.wakers.add(wake);
    });
  }

  /**
   * Resolve true when a disconnect callback arrives within timeoutMs,
   * false on timeout. Throws StopError when stop() is called meanwhile.
   */
  waitForDisconnect(timeoutMs: number): Promise<boolean> {
    this.check();
    return new Promise((resolve, reject) => {
      let done = false;
      const finish = (fn: () => void) => {
        if (done) return;
        done = true;
        clearTimeout(timer);
        this.wakers.delete(wake);
        this.disconnectWaiters = this.disconnectWaiters.filter((w) => w !== onDisc);
        fn();
      };
      const timer = window.setTimeout(() => finish(() => resolve(false)), Math.max(0, timeoutMs));
      const onDisc = () => finish(() => resolve(true));
      const wake = () => finish(() => reject(new StopError()));
      this.disconnectWaiters.push(onDisc);
      this.wakers.add(wake);
    });
  }

  /** Poll the connection-state ref until it equals `expected` or time is up. */
  async waitForState(expected: boolean, timeoutMs: number): Promise<boolean> {
    const deadline = Date.now() + timeoutMs;
    while (this.connected.value !== expected) {
      if (Date.now() >= deadline) return false;
      await this.sleep(20);
    }
    return true;
  }

  // ---- instrumented plugin calls -----------------------------------------

  /**
   * Run a plugin call, measure it, classify failures (timeout vs. error),
   * count and log them. Rethrows as OpError; StopError passes through.
   */
  async op<T>(name: string, fn: () => Promise<T>, quiet = false): Promise<T> {
    const start = performance.now();
    try {
      const res = await fn();
      const dur = Math.round(performance.now() - start);
      if (!quiet) log.info(`${name} ok (${dur} ms)`);
      return res;
    } catch (e) {
      if (e instanceof StopError) throw e;
      const dur = Math.round(performance.now() - start);
      const msg = errStr(e);
      const timeout = isTimeoutError(msg);
      if (timeout) {
        this.counters.opTimeouts++;
        log.error(`${name} TIMEOUT after ${dur} ms: ${msg}`);
      } else {
        this.counters.opErrors++;
        log.error(`${name} FAILED after ${dur} ms: ${msg}`);
      }
      throw new OpError(name, msg, timeout);
    }
  }

  /**
   * Like op(), but the call is expected to fail (server armed CONTROL 0x06).
   * A plain error is the good case, a timeout is still counted, and an
   * unexpected success is reported.
   */
  async opExpectFail(
    name: string,
    fn: () => Promise<unknown>
  ): Promise<{ outcome: "failed" | "timeout" | "succeeded"; detail: string }> {
    const start = performance.now();
    try {
      await fn();
      const dur = Math.round(performance.now() - start);
      log.warn(`${name}: expected an ATT error but the call succeeded (${dur} ms)`);
      return { outcome: "succeeded", detail: `succeeded after ${dur} ms` };
    } catch (e) {
      if (e instanceof StopError) throw e;
      const dur = Math.round(performance.now() - start);
      const msg = errStr(e);
      if (isTimeoutError(msg)) {
        this.counters.opTimeouts++;
        log.error(`${name}: expected an ATT error but got TIMEOUT after ${dur} ms: ${msg}`);
        return { outcome: "timeout", detail: `timeout after ${dur} ms: ${msg}` };
      }
      log.info(`${name}: failed as expected (${dur} ms): ${msg}`);
      return { outcome: "failed", detail: `${msg} (${dur} ms)` };
    }
  }

  // ---- scanning -------------------------------------------------------------

  private matchesTarget(dev: BleDevice): boolean {
    return !!dev.name && dev.name.includes(this._cfg.nameFilter);
  }

  /** Advertisement actually received during this scan (not a cached entry). */
  private isSeen(dev: BleDevice): boolean {
    return (dev.rssi as number | null) != null;
  }

  /** Forget the target so the next connect scans again. */
  forgetTarget() {
    this.target = null;
  }

  /**
   * Scan until a device whose name contains the filter shows up (with an
   * RSSI). Rejects when the scan times out without a match.
   */
  scanForTarget(timeoutMs = this._cfg.scanTimeoutMs): Promise<BleDevice> {
    this.check();
    this.counters.scans++;
    return this.op(`scan for "${this._cfg.nameFilter}" (${timeoutMs} ms)`, () => {
      return new Promise<BleDevice>((resolve, reject) => {
        let done = false;
        const finish = (fn: () => void) => {
          if (done) return;
          done = true;
          clearTimeout(timer);
          this.wakers.delete(wake);
          this.scanning = false;
          stopScan().catch(() => {});
          fn();
        };
        const timer = window.setTimeout(
          () => finish(() => reject(new Error(`target not found within ${timeoutMs} ms`))),
          timeoutMs + 1000
        );
        const wake = () => finish(() => reject(new StopError()));
        this.wakers.add(wake);
        this.scanning = true;
        startScan((devices) => {
          const dev = devices.find((d) => this.matchesTarget(d) && this.isSeen(d));
          if (dev) {
            finish(() => {
              log.info(`found ${dev.name} ${dev.address} rssi=${dev.rssi}`);
              this.target = dev;
              resolve(dev);
            });
          }
        }, timeoutMs).catch((e) => finish(() => reject(e)));
      });
    });
  }

  /**
   * Scan for `durationMs` and report whether the target was seen (with an
   * RSSI, i.e. actually advertising) at any point.
   */
  scanSnapshot(durationMs: number): Promise<{ seen: boolean; devices: number; rssi: number | null }> {
    this.check();
    this.counters.scans++;
    return this.op(`scan snapshot ${durationMs} ms`, () => {
      return new Promise((resolve, reject) => {
        const all = new Map<string, BleDevice>();
        let seen = false;
        let rssi: number | null = null;
        let done = false;
        const finish = (fn: () => void) => {
          if (done) return;
          done = true;
          clearTimeout(timer);
          this.wakers.delete(wake);
          this.scanning = false;
          stopScan().catch(() => {});
          fn();
        };
        const timer = window.setTimeout(
          () => finish(() => resolve({ seen, devices: all.size, rssi })),
          durationMs + 500
        );
        const wake = () => finish(() => reject(new StopError()));
        this.wakers.add(wake);
        this.scanning = true;
        startScan((devices) => {
          for (const d of devices) {
            all.set(d.address, d);
            if (this.matchesTarget(d) && this.isSeen(d)) {
              seen = true;
              rssi = d.rssi;
            }
          }
        }, durationMs).catch((e) => finish(() => reject(e)));
      });
    });
  }

  private async ensureTarget(): Promise<BleDevice> {
    if (!this.target) this.target = await this.scanForTarget();
    return this.target;
  }

  // ---- connection management ----------------------------------------------------

  private onDisconnectCallback() {
    this.counters.disconnectCallbacks++;
    this._connCallbacks++;
    if (this.expectDisconnect) {
      this.counters.disconnectsExpected++;
      log.info(`disconnect callback (expected, #${this._connCallbacks} for this connection)`);
    } else {
      this.counters.disconnectsUnexpected++;
      log.warn(`disconnect callback: UNEXPECTED disconnect (#${this._connCallbacks} for this connection)`);
    }
    this.counterSeqExpected = null;
    this.subscribedEcho = false;
    this.subscribedCounter = false;
    const waiters = this.disconnectWaiters;
    this.disconnectWaiters = [];
    for (const w of waiters) w();
  }

  /**
   * Announce that the server is about to drop the link on our request, so
   * the callback counts as expected. Returns the callback count to compare
   * against with callbacksSince().
   */
  expectServerDrop(): number {
    this.expectDisconnect = true;
    return this._connCallbacks;
  }

  /** Disconnect callbacks for the current connection since `before`. */
  callbacksSince(before: number): number {
    return this._connCallbacks - before;
  }

  /** connect() with the disconnect callback wired up; counts connectOk/Fail. */
  private async connectTo(dev: BleDevice) {
    this.check();
    this._connCallbacks = 0;
    this.expectDisconnect = false;
    this.echoSeqExpected = 0;
    this.echoQueue.clear();
    this.subscribedEcho = false;
    this.subscribedCounter = false;
    try {
      await this.op(`connect ${dev.address}`, () =>
        connect(dev.address, () => this.onDisconnectCallback())
      );
      this.counters.connectOk++;
    } catch (e) {
      this.counters.connectFail++;
      this.expectDisconnect = true;
      throw e;
    }
    try {
      const mtu = await getMtu();
      log.info(`mtu=${mtu}`);
    } catch (e) {
      log.warn(`getMtu failed: ${errStr(e)}`);
    }
  }

  /**
   * Connect to the target, scanning first when no target is known and
   * rescanning when the plugin does not know the address any more (its
   * device list is cleared by every scan).
   */
  async connectTarget() {
    const dev = await this.ensureTarget();
    try {
      await this.connectTo(dev);
    } catch (e) {
      if (e instanceof OpError && isUnknownPeripheral(e.detail)) {
        log.info("peripheral unknown to the plugin, rescanning");
        this.target = await this.scanForTarget();
        await this.connectTo(this.target);
      } else {
        throw e;
      }
    }
  }

  /**
   * Retry connectTarget() (with backoff) until it succeeds or `boundMs` is
   * exhausted. Returns false when the bound was hit.
   */
  async reconnectWithin(boundMs: number): Promise<boolean> {
    const start = Date.now();
    let attempt = 0;
    do {
      this.check();
      attempt++;
      try {
        await this.connectTarget();
        log.info(`reconnected after ${Date.now() - start} ms (${attempt} attempt(s))`);
        return true;
      } catch (e) {
        if (e instanceof StopError) throw e;
        if (attempt % 2 === 0) {
          // every second failure: refresh the address via a scan
          try {
            this.target = await this.scanForTarget();
          } catch (se) {
            if (se instanceof StopError) throw se;
          }
        }
        if (Date.now() - start + this._cfg.reconnectBackoffMs >= boundMs) break;
        await this.sleep(this._cfg.reconnectBackoffMs);
      }
    } while (Date.now() - start < boundMs);
    log.error(`reconnect did not succeed within ${boundMs} ms (${attempt} attempt(s))`);
    return false;
  }

  /**
   * User-initiated disconnect; waits up to `callbackWindowMs` for the
   * callback. Returns true when a callback arrived, false otherwise
   * (counted as opError).
   */
  async disconnectExpected(callbackWindowMs = 2000): Promise<boolean> {
    this.expectDisconnect = true;
    const before = this._connCallbacks;
    await this.op("disconnect", () => disconnect());
    // The callback is delivered through a channel and may trail the command.
    const got = before < this._connCallbacks || (await this.waitForDisconnect(callbackWindowMs));
    if (!got) {
      this.counters.opErrors++;
      log.error(`no disconnect callback within ${callbackWindowMs} ms after disconnect()`);
    }
    return got;
  }

  /** After a failed step: make sure we are disconnected before continuing. */
  async recoverDisconnected() {
    if (!this.connected.value) return;
    this.expectDisconnect = true;
    try {
      await this.op("disconnect (recovery)", () => disconnect());
    } catch (e) {
      if (e instanceof StopError) throw e;
    }
  }

  // ---- subscriptions / baseline ------------------------------------------------

  async subscribeEcho() {
    if (this.subscribedEcho) return;
    this.echoQueue.clear();
    await this.op("subscribe ECHO", () =>
      subscribe(ECHO_UUID, SERVICE_UUID, (data) => {
        this.counters.notifications++;
        this.echoQueue.push(data);
      })
    );
    this.subscribedEcho = true;
  }

  async subscribeCounter() {
    if (this.subscribedCounter) return;
    this.counterSeqExpected = 0;
    await this.op("subscribe COUNTER", () =>
      subscribe(COUNTER_UUID, SERVICE_UUID, (data) => this.onCounter(data))
    );
    this.subscribedCounter = true;
  }

  /** Subscribe to ECHO and COUNTER (the baseline subscriptions). */
  async restoreSubscriptions() {
    await this.subscribeCounter();
    await this.subscribeEcho();
  }

  /**
   * Baseline state before a script: connected and subscribed to ECHO and
   * COUNTER. Scans/reconnects as needed; throws when that is impossible
   * within one connect + scan window.
   */
  async ensureBaseline() {
    if (!this.connected.value) {
      const bound = this._cfg.timeouts.connect + this._cfg.scanTimeoutMs + 5000;
      if (!(await this.reconnectWithin(bound))) {
        throw new Error("baseline: could not connect to the target");
      }
    }
    await this.restoreSubscriptions();
  }

  private onCounter(data: number[]) {
    this.counters.notifications++;
    this.counterNotifications++;
    if (data.length < 4) {
      this.counters.opErrors++;
      log.error(`COUNTER frame too short: ${hex(data)}`);
      return;
    }
    const seq = readU32le(data);
    if (this.counterSeqExpected !== null && seq !== this.counterSeqExpected) {
      this.counters.seqGaps++;
      this.counterSeqGaps++;
      log.warn(`COUNTER seq gap: expected ${this.counterSeqExpected}, got ${seq}`);
    }
    this.counterSeqExpected = seq + 1;
  }

  // ---- echo ------------------------------------------------------------------------

  /** 20 byte random payload with iteration/index marker (see protocol.ts). */
  makeEchoPayload(iteration: number, index: number): number[] {
    return makePayload(SCRIPT_ECHO_SIZE, iteration, index);
  }

  /** Write one payload to ECHO (counted/logged through op()). */
  async sendEcho(
    payload: number[],
    writeType: "withResponse" | "withoutResponse",
    label: string
  ): Promise<void> {
    await this.op(`send ECHO ${label}`, () => send(ECHO_UUID, payload, writeType, SERVICE_UUID), true);
  }

  /**
   * Await the next echoed frame and verify it: `[seq u32 LE][payload]`, seq
   * one higher than the previous echo of this connection, payload equal.
   */
  async awaitEcho(payload: number[], label: string, startedAt = performance.now()): Promise<EchoResult> {
    let frame: number[];
    try {
      frame = await this.echoQueue.next(this._cfg.timeouts.write);
    } catch (e) {
      this.counters.opTimeouts++;
      const detail = `no echo notification: ${errStr(e)}`;
      log.error(`echo ${label}: ${detail}`);
      return { ok: false, rtt: Math.round(performance.now() - startedAt), detail };
    }
    const rtt = Math.round(performance.now() - startedAt);
    if (frame.length < 4) {
      this.counters.echoMismatches++;
      const detail = `frame too short: ${hex(frame)}`;
      log.error(`echo ${label}: ${detail}`);
      return { ok: false, rtt, detail };
    }
    const seq = readU32le(frame);
    const body = frame.slice(4);
    const problems: string[] = [];
    if (seq !== this.echoSeqExpected) {
      this.counters.seqGaps++;
      problems.push(`seq gap: expected ${this.echoSeqExpected}, got ${seq}`);
    }
    this.echoSeqExpected = seq + 1;
    if (!bytesEqual(body, payload)) {
      this.counters.echoMismatches++;
      problems.push(`payload mismatch: sent ${hex(payload)} got ${hex(body)}`);
    }
    if (problems.length > 0) {
      const detail = problems.join("; ");
      log.error(`echo ${label}: ${detail}`);
      return { ok: false, rtt, detail };
    }
    return { ok: true, rtt, detail: `seq ${seq}, ${rtt} ms` };
  }

  /** One echo write followed by verification of the echoed frame. */
  async echoOnce(
    iteration: number,
    index: number,
    writeType: "withResponse" | "withoutResponse" = "withResponse"
  ): Promise<EchoResult> {
    this.check();
    const payload = makePayload(SCRIPT_ECHO_SIZE, iteration, index);
    const start = performance.now();
    const label = `#${iteration}/${index}`;
    try {
      await this.sendEcho(payload, writeType, label);
    } catch (e) {
      if (e instanceof StopError) throw e;
      return { ok: false, rtt: Math.round(performance.now() - start), detail: `write failed: ${errStr(e)}` };
    }
    return this.awaitEcho(payload, label, start);
  }

  /** `count` sequential echo writes; summarises the outcome. */
  async echoSeries(
    count: number,
    iteration: number,
    writeType: "withResponse" | "withoutResponse" = "withResponse"
  ): Promise<EchoSeries> {
    const results: EchoResult[] = [];
    let okCount = 0;
    let minRtt = Infinity;
    let maxRtt = 0;
    for (let i = 0; i < count; i++) {
      const r = await this.echoOnce(iteration, i, writeType);
      results.push(r);
      if (r.ok) okCount++;
      minRtt = Math.min(minRtt, r.rtt);
      maxRtt = Math.max(maxRtt, r.rtt);
    }
    const firstBad = results.find((r) => !r.ok);
    const detail =
      `${okCount}/${count} verified, rtt ${count > 0 ? minRtt : 0}-${maxRtt} ms` +
      (firstBad ? `; first failure: ${firstBad.detail}` : "");
    return { okCount, results, minRtt: count > 0 ? minRtt : 0, maxRtt, detail };
  }

  // ---- reads / control -----------------------------------------------------------

  async readStatus(): Promise<ServerStatus> {
    const raw = await this.op("read STATUS", () => read(STATUS_UUID, SERVICE_UUID), true);
    const text = decodeUtf8(raw);
    try {
      const status = JSON.parse(text) as ServerStatus;
      log.info(`STATUS ${text}`);
      return status;
    } catch {
      this.counters.opErrors++;
      throw new Error(`STATUS is not valid JSON: ${text}`);
    }
  }

  async readReport(): Promise<Report> {
    const raw = await this.op("read REPORT", () => read(REPORT_UUID, SERVICE_UUID), true);
    const text = decodeUtf8(raw);
    try {
      return JSON.parse(text) as Report;
    } catch {
      this.counters.opErrors++;
      throw new Error(`REPORT is not valid JSON: ${text.slice(0, 200)}`);
    }
  }

  /** Read LARGE and verify byte i == i & 0xff. */
  async readLargeVerified(): Promise<{ ok: boolean; detail: string }> {
    const data = await this.op("read LARGE", () => read(LARGE_UUID, SERVICE_UUID), true);
    let bad = 0;
    for (let i = 0; i < data.length; i++) if (data[i] !== (i & 0xff)) bad++;
    if (data.length === 0) return { ok: false, detail: "empty read" };
    if (bad > 0) {
      this.counters.opErrors++;
      return { ok: false, detail: `${bad} of ${data.length} bytes wrong` };
    }
    return { ok: true, detail: `${data.length} bytes verified` };
  }

  /** LARGE read that is expected to fail with an ATT error. */
  readLargeExpectFail(label: string) {
    return this.opExpectFail(label, () => read(LARGE_UUID, SERVICE_UUID));
  }

  async sendControl(label: string, bytes: number[]) {
    await this.op(`CONTROL ${label} [${hex(bytes)}]`, () =>
      send(CONTROL_UUID, bytes, "withResponse", SERVICE_UUID)
    );
  }

  // ---- scenario: integration -------------------------------------------------------

  private async runIntegration() {
    const sel = this._cfg.script;
    const ids = sel === "all" ? RUN_ALL_ORDER : [sel];
    this.results.splice(0, this.results.length);
    for (const id of ids) {
      const def = SCRIPTS.find((s) => s.id === id);
      if (!def) throw new Error(`unknown script id ${id}`);
      this.results.push({
        id: def.id,
        name: def.name,
        status: "pending",
        verdict: null,
        clientSteps: def.clientSteps.map((name) => ({ name, ok: null, detail: "pending" })),
        serverSteps: def.serverSteps.map((name) => ({ name, ok: null, detail: "pending" })),
        startedAt: 0,
        durationMs: 0,
        serverState: "",
        error: "",
      });
    }

    let passed = 0;
    for (const result of this.results) {
      this.check();
      const def = SCRIPTS.find((s) => s.id === result.id)!;
      if (await runScript(this, def, result)) passed++;
      this.counters.iterations++;
    }
    log.info(`integration: ${passed}/${ids.length} scripts passed`);
  }

  // ---- scenario: hold ---------------------------------------------------------------

  private async runHold() {
    await this.ensureBaseline();
    let lastReport = Date.now();
    let lastNotifications = this.counterNotifications;
    for (;;) {
      this.check();
      const dropped = await this.waitForDisconnect(10000);
      if (!dropped) {
        const now = Date.now();
        const rate = ((this.counterNotifications - lastNotifications) * 1000) / (now - lastReport);
        log.info(
          `hold: connected, ${this.counterNotifications} COUNTER notifications (${rate.toFixed(1)}/s), ` +
            `next seq ${this.counterSeqExpected}, gaps ${this.counterSeqGaps}`
        );
        lastReport = now;
        lastNotifications = this.counterNotifications;
        continue;
      }
      this.counters.iterations++;
      log.warn(`hold: link lost, reconnecting in ${this._cfg.reconnectBackoffMs} ms`);
      await this.sleep(this._cfg.reconnectBackoffMs);
      // Retry until stopped; each round is bounded so progress is logged.
      while (!(await this.reconnectWithin(this._cfg.timeouts.connect + this._cfg.scanTimeoutMs + 5000))) {
        this.check();
      }
      await this.restoreSubscriptions();
    }
  }
}

export const runner = new StressRunner();
