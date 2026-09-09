// Integration scripts: the client half of the timelines in
// ../../stress-protocol.md ("Integration scripts"). Every script definition
// lists its client steps (exact names from the spec tables) and drives the
// StressRunner; the server half is fetched from the REPORT characteristic
// afterwards and merged into the result.

import { listServices } from "@mnlphlp/plugin-blec";
import { errStr, StopError } from "./errors";
import { log } from "./log";
import {
  ALL_CHARACTERISTICS,
  CTRL_SCRIPT_START,
  SERVICE_UUID,
  type Report,
  type ReportState,
} from "./protocol";
import type { StressRunner } from "./stress";

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

export type StepResult = {
  name: string;
  /** null while pending */
  ok: boolean | null;
  detail: string;
};

export type ScriptStatus = "pending" | "running" | "done";

export type ScriptResult = {
  id: number;
  name: string;
  status: ScriptStatus;
  verdict: "PASS" | "FAIL" | null;
  clientSteps: StepResult[];
  serverSteps: StepResult[];
  startedAt: number;
  durationMs: number;
  /** REPORT state at the end of polling ("" until read once) */
  serverState: ReportState | "";
  /** engine-level error (baseline not reached, exception, ...) */
  error: string;
};

export type ScriptSelection = "all" | number;

/** Order used by "run all". */
export const RUN_ALL_ORDER = [1, 4, 5, 2, 3, 6, 7];

export type ScriptDef = {
  id: number;
  name: string;
  nominalMs: number;
  clientSteps: string[];
  serverSteps: string[];
  run: (r: ScriptRun) => Promise<void>;
};

// ---------------------------------------------------------------------------
// Per-run helper: timeline relative to t0 and step bookkeeping
// ---------------------------------------------------------------------------

export class ScriptRun {
  /** Wall-clock time when the CONTROL 0x10 write returned. */
  t0 = 0;

  constructor(
    readonly ctx: StressRunner,
    readonly def: ScriptDef,
    readonly result: ScriptResult
  ) {}

  /** Milliseconds since t0. */
  get elapsed(): number {
    return Date.now() - this.t0;
  }

  /** Milliseconds until the given offset from t0 (0 when already past). */
  until(offsetMs: number): number {
    return Math.max(0, this.t0 + offsetMs - Date.now());
  }

  /** Sleep until the given offset from t0. */
  async at(offsetMs: number) {
    const ms = this.until(offsetMs);
    if (ms > 0) await this.ctx.sleep(ms);
  }

  fmt(ms: number): string {
    return `${(ms / 1000).toFixed(2)} s`;
  }

  /** Resolve a client step and log it. */
  set(name: string, ok: boolean, detail: string) {
    const step = this.result.clientSteps.find((s) => s.name === name);
    if (!step) {
      log.error(`internal: unknown client step ${this.def.name}/${name}`);
      return;
    }
    step.ok = ok;
    step.detail = detail;
    log[ok ? "info" : "error"](`${ok ? "PASS" : "FAIL"} ${this.def.name}/client/${name} ${detail}`);
  }

  /**
   * Run `fn` for a step; it returns [ok, detail]. Exceptions (except a user
   * stop) fail the step with the error text.
   */
  async step(name: string, fn: () => Promise<[boolean, string]>) {
    try {
      const [ok, detail] = await fn();
      this.set(name, ok, detail);
      return ok;
    } catch (e) {
      if (e instanceof StopError) throw e;
      this.set(name, false, errStr(e));
      return false;
    }
  }

  /** `count` verified echo writes as one step. */
  async echoStep(name: string, count: number) {
    return this.step(name, async () => {
      if (!this.ctx.connected.value) return [false, "not connected"];
      const s = await this.ctx.echoSeries(count, this.def.id);
      return [s.okCount === count, s.detail];
    });
  }

  /**
   * Reconnect (scan if needed) and restore the baseline subscriptions before
   * `deadlineOffset` from t0. Returns [ok, detail].
   */
  async reconnectBy(deadlineOffset: number): Promise<[boolean, string]> {
    if (!(await this.ctx.reconnectWithin(this.until(deadlineOffset)))) {
      return [false, `not reconnected by ${this.fmt(deadlineOffset)}`];
    }
    const tConn = this.elapsed;
    await this.ctx.restoreSubscriptions();
    const tSub = this.elapsed;
    const inTime = tSub <= deadlineOffset;
    return [inTime, `reconnected at ${this.fmt(tConn)}, resubscribed at ${this.fmt(tSub)}${inTime ? "" : " (late)"}`];
  }

  /**
   * Wait for the server-initiated drop. Returns the callback offset (or
   * null) and the baseline for callbacksSince().
   */
  async awaitServerDrop(windowEnd: number): Promise<{ before: number; at: number | null }> {
    const before = this.ctx.expectServerDrop();
    const got = await this.ctx.waitForDisconnect(this.until(windowEnd));
    return { before, at: got ? this.elapsed : null };
  }

  /**
   * Verdict for an "exactly one disconnect callback between `from` and
   * `to`" step, evaluated after `to` has passed. The lower bound is checked
   * with a 500 ms tolerance because the server's t0 (receipt of the write)
   * is earlier than ours (return of the write).
   */
  callbackVerdict(drop: { before: number; at: number | null }, from: number, to: number): [boolean, string] {
    const n = this.ctx.callbacksSince(drop.before);
    if (drop.at === null) return [false, `no disconnect callback by ${this.fmt(to)} (${n} callbacks)`];
    const early = drop.at < from - 500;
    const ok = n === 1 && !early;
    return [
      ok,
      `callback at ${this.fmt(drop.at)}, ${n} callback(s) by ${this.fmt(this.elapsed)}` +
        (early ? ` (earlier than ${this.fmt(from)})` : ""),
    ];
  }
}

// ---------------------------------------------------------------------------
// Script definitions
// ---------------------------------------------------------------------------

const echoBaseline: ScriptDef = {
  id: 1,
  name: "echo-baseline",
  nominalMs: 10000,
  clientSteps: ["echo_with_response", "echo_without_response"],
  serverSteps: ["echo_writes_received"],
  async run(r) {
    await r.echoStep("echo_with_response", 20);
    await r.step("echo_without_response", async () => {
      // Burst all writes first, then verify the echoed frames in order.
      const count = 20;
      const payloads: number[][] = [];
      let sendError = "";
      for (let i = 0; i < count; i++) {
        r.ctx.check();
        const payload = r.ctx.makeEchoPayload(r.def.id, 100 + i);
        try {
          await r.ctx.sendEcho(payload, "withoutResponse", `#${r.def.id}/${100 + i} (no response)`);
          payloads.push(payload);
        } catch (e) {
          if (e instanceof StopError) throw e;
          sendError = errStr(e);
          break;
        }
      }
      let verified = 0;
      let firstBad = "";
      for (let i = 0; i < payloads.length; i++) {
        const res = await r.ctx.awaitEcho(payloads[i], `#${r.def.id}/${100 + i} (no response)`);
        if (res.ok) verified++;
        else if (!firstBad) firstBad = `frame ${i}: ${res.detail}`;
      }
      const detail =
        `${payloads.length}/${count} writes sent, ${verified}/${count} frames verified in order` +
        (sendError ? `; write failed: ${sendError}` : "") +
        (firstBad ? `; first failure: ${firstBad}` : "");
      return [verified === count && !sendError, detail];
    });
  },
};

const linkLoss: ScriptDef = {
  id: 2,
  name: "link-loss",
  nominalMs: 20000,
  clientSteps: ["disconnect_callback", "state_false", "reconnect", "echo_after_reconnect"],
  serverSteps: ["drop", "client_reconnected", "echo_after_reconnect"],
  async run(r) {
    const drop = await r.awaitServerDrop(5000);
    let stateFalse = false;
    if (drop.at !== null) stateFalse = await r.ctx.waitForState(false, r.until(6000));
    await r.at(6000);
    r.set("disconnect_callback", ...r.callbackVerdict(drop, 2000, 5000));
    const connectedNow = r.ctx.connected.value;
    r.set(
      "state_false",
      !connectedNow,
      connectedNow
        ? `connection state still true at ${r.fmt(r.elapsed)}`
        : `connection state false at ${r.fmt(r.elapsed)}${stateFalse ? " (seen after the callback)" : ""}`
    );
    const reconnected = await r.step("reconnect", () => r.reconnectBy(17000));
    if (reconnected) await r.echoStep("echo_after_reconnect", 5);
    else r.set("echo_after_reconnect", false, "skipped: not reconnected");
  },
};

const disappear: ScriptDef = {
  id: 3,
  name: "disappear",
  nominalMs: 25000,
  clientSteps: ["disconnect_callback", "not_visible", "visible_again", "reconnect", "echo_after_reconnect"],
  serverSteps: ["hide", "show", "client_reconnected", "echo_after_reconnect"],
  async run(r) {
    const drop = await r.awaitServerDrop(4000);
    if (drop.at !== null) {
      await r.step("not_visible", async () => {
        const snap = await r.ctx.scanSnapshot(3000);
        return [
          !snap.seen,
          snap.seen
            ? `target reported with rssi=${snap.rssi} while hidden (scan ${r.fmt(drop.at!)}-${r.fmt(r.elapsed)})`
            : `target not seen in ${snap.devices} device(s) (scan ${r.fmt(drop.at!)}-${r.fmt(r.elapsed)})`,
        ];
      });
    } else {
      r.set("not_visible", false, "skipped: no disconnect callback");
    }
    await r.at(4000);
    r.set("disconnect_callback", ...r.callbackVerdict(drop, 1000, 4000));

    await r.at(8000);
    await r.step("visible_again", async () => {
      r.ctx.forgetTarget();
      const started = r.elapsed;
      const dev = await r.ctx.scanForTarget(Math.min(10000, r.ctx.cfg.scanTimeoutMs));
      return [true, `seen at ${r.fmt(r.elapsed)} rssi=${dev.rssi} (scan started ${r.fmt(started)})`];
    });
    const reconnected = await r.step("reconnect", () => r.reconnectBy(22000));
    if (reconnected) await r.echoStep("echo_after_reconnect", 5);
    else r.set("echo_after_reconnect", false, "skipped: not reconnected");
  },
};

const notificationStorm: ScriptDef = {
  id: 4,
  name: "notification-storm",
  nominalMs: 8000,
  clientSteps: ["echo_during_storm", "storm_received", "no_seq_gaps"],
  serverSteps: ["storm"],
  async run(r) {
    const gapsAtStart = r.ctx.counterSeqGaps;
    await r.at(1000);
    const countAt1s = r.ctx.counterNotifications;
    await r.at(1500);
    await r.echoStep("echo_during_storm", 5);
    await r.at(8000);
    const received = r.ctx.counterNotifications - countAt1s;
    r.set("storm_received", received >= 500, `${received} COUNTER notifications between 1 s and 8 s (need 500)`);
    const gaps = r.ctx.counterSeqGaps - gapsAtStart;
    r.set("no_seq_gaps", gaps === 0, `${gaps} COUNTER seq gap(s) during the script`);
  },
};

const slowAndFail: ScriptDef = {
  id: 5,
  name: "slow-and-fail",
  nominalMs: 15000,
  clientSteps: ["slow_echo", "reads_fail", "read_ok", "status_ok"],
  serverSteps: ["slow_on", "slow_echo_received", "slow_off_fail_armed", "fails_consumed"],
  async run(r) {
    await r.at(500);
    await r.step("slow_echo", async () => {
      const s = await r.ctx.echoSeries(3, r.def.id);
      const slowEnough = s.results.every((e) => e.rtt >= 1500);
      const rtts = s.results.map((e) => e.rtt).join("/");
      return [s.okCount === 3 && slowEnough, `${s.okCount}/3 verified, rtt ${rtts} ms (need >= 1500 each)`];
    });
    await r.at(8500);
    await r.step("reads_fail", async () => {
      const outcomes: string[] = [];
      let ok = true;
      for (let i = 0; i < 2; i++) {
        r.ctx.check();
        const res = await r.ctx.readLargeExpectFail(`read LARGE (expected failure ${i + 1}/2)`);
        outcomes.push(`${i + 1}: ${res.outcome} - ${res.detail}`);
        if (res.outcome !== "failed") ok = false;
      }
      return [ok, outcomes.join("; ")];
    });
    await r.step("read_ok", async () => {
      const res = await r.ctx.readLargeVerified();
      return [res.ok, res.detail];
    });
    await r.step("status_ok", async () => {
      const status = await r.ctx.readStatus();
      return [status.fail_ops_left === 0, `fail_ops_left=${status.fail_ops_left} slow_ms=${status.slow_ms}`];
    });
  },
};

const restart: ScriptDef = {
  id: 6,
  name: "restart",
  nominalMs: 20000,
  clientSteps: ["disconnect_callback", "reconnect", "echo_after_restart"],
  serverSteps: ["down", "up", "client_reconnected", "echo_after_restart"],
  async run(r) {
    const drop = await r.awaitServerDrop(5000);
    await r.at(5000);
    r.set("disconnect_callback", ...r.callbackVerdict(drop, 1000, 5000));
    const reconnected = await r.step("reconnect", async () => {
      // "scan first": the server re-registered everything, do not trust the
      // cached address.
      r.ctx.forgetTarget();
      if (!(await r.ctx.reconnectWithin(r.until(18000)))) return [false, `not reconnected by ${r.fmt(18000)}`];
      const tConn = r.elapsed;
      const address = r.ctx.targetAddress!;
      const services = await r.ctx.op("listServices", () => listServices(address), true);
      if (typeof services === "string") return [false, `listServices returned: ${services}`];
      const stress = services.find((s) => s.uuid.toLowerCase() === SERVICE_UUID);
      if (!stress) return [false, `STRESS service missing (${services.length} services)`];
      const have = new Set(stress.characteristics.map((c) => c.uuid.toLowerCase()));
      const missing = ALL_CHARACTERISTICS.filter((u) => !have.has(u));
      await r.ctx.restoreSubscriptions();
      const tSub = r.elapsed;
      const ok = missing.length === 0 && tSub <= 18000;
      return [
        ok,
        `reconnected at ${r.fmt(tConn)}, ${have.size} characteristics` +
          (missing.length ? ` (missing ${missing.map((u) => u.slice(9, 13)).join(",")})` : "") +
          `, resubscribed at ${r.fmt(tSub)}${tSub > 18000 ? " (late)" : ""}`,
      ];
    });
    if (reconnected) await r.echoStep("echo_after_restart", 5);
    else r.set("echo_after_restart", false, "skipped: not reconnected");
  },
};

const rapidReconnect: ScriptDef = {
  id: 7,
  name: "rapid-reconnect",
  nominalMs: 60000,
  clientSteps: ["cycles", "state_tracking"],
  serverSteps: ["connects_seen", "echo_seen"],
  async run(r) {
    const total = 10;
    let cyclesOk = 0;
    let stateOk = true;
    const stateProblems: string[] = [];
    const cycleProblems: string[] = [];
    for (let c = 0; c < total; c++) {
      r.ctx.check();
      if (r.elapsed > 60000) {
        cycleProblems.push(`cycle ${c + 1}: window elapsed`);
        break;
      }
      const cbBefore = r.ctx.counters.disconnectCallbacks;
      let ok = false;
      try {
        if (!r.ctx.connected.value) throw new Error("not connected at cycle start");
        const gotCb = await r.ctx.disconnectExpected(3000);
        if (!(await r.ctx.waitForState(false, 1000))) {
          stateOk = false;
          stateProblems.push(`cycle ${c + 1}: state not false after disconnect`);
        }
        await r.ctx.connectTarget();
        if (!(await r.ctx.waitForState(true, 1000))) {
          stateOk = false;
          stateProblems.push(`cycle ${c + 1}: state not true after connect`);
        }
        await r.ctx.subscribeEcho();
        const echo = await r.ctx.echoOnce(r.def.id, c);
        const callbacks = r.ctx.counters.disconnectCallbacks - cbBefore;
        ok = gotCb && callbacks === 1 && echo.ok;
        if (!ok) {
          cycleProblems.push(
            `cycle ${c + 1}: ` +
              [
                !gotCb ? "no disconnect callback within 3 s" : "",
                callbacks !== 1 ? `${callbacks} disconnect callbacks` : "",
                !echo.ok ? `echo: ${echo.detail}` : "",
              ]
                .filter(Boolean)
                .join(", ")
          );
        }
      } catch (e) {
        if (e instanceof StopError) throw e;
        cycleProblems.push(`cycle ${c + 1}: ${errStr(e)}`);
        if (!r.ctx.connected.value) {
          // get back to a connected state so the next cycle can run
          await r.ctx.reconnectWithin(r.until(60000));
          await r.ctx.subscribeEcho().catch(() => {});
        }
      }
      if (ok) cyclesOk++;
      log.info(`rapid-reconnect cycle ${c + 1}/${total} ${ok ? "ok" : "FAILED"} at ${r.fmt(r.elapsed)}`);
    }
    const inTime = r.elapsed <= 60000;
    r.set(
      "cycles",
      cyclesOk === total && inTime,
      `${cyclesOk}/${total} cycles ok in ${r.fmt(r.elapsed)}` +
        (inTime ? "" : " (over 60 s)") +
        (cycleProblems.length ? `; ${cycleProblems.slice(0, 3).join("; ")}` : "")
    );
    r.set(
      "state_tracking",
      stateOk,
      stateOk ? "state false after every disconnect and true after every connect" : stateProblems.slice(0, 3).join("; ")
    );
  },
};

export const SCRIPTS: ScriptDef[] = [
  echoBaseline,
  linkLoss,
  disappear,
  notificationStorm,
  slowAndFail,
  restart,
  rapidReconnect,
];

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/**
 * Run one script end to end: restore the baseline, start it on the server,
 * execute the client steps, poll REPORT, merge and log the verdict.
 * Returns true when the script passed. StopError propagates.
 */
export async function runScript(ctx: StressRunner, def: ScriptDef, result: ScriptResult): Promise<boolean> {
  result.status = "running";
  result.startedAt = Date.now();
  result.durationMs = 0;
  result.verdict = null;
  result.error = "";
  result.serverState = "";
  for (const s of result.clientSteps) {
    s.ok = null;
    s.detail = "pending";
  }
  for (const s of result.serverSteps) {
    s.ok = null;
    s.detail = "pending";
  }
  log.info(`=== script ${def.id} ${def.name} (nominal ${def.nominalMs / 1000} s)`);

  const run = new ScriptRun(ctx, def, result);
  let started = false;
  try {
    await ctx.ensureBaseline();
    await ctx.sendControl(`start script ${def.id} ${def.name}`, [CTRL_SCRIPT_START, def.id]);
    run.t0 = Date.now();
    started = true;
    await def.run(run);
  } catch (e) {
    if (e instanceof StopError) {
      result.error = "stopped by user";
      finish(result, def, run);
      throw e;
    }
    result.error = started ? `script aborted: ${errStr(e)}` : `baseline not reached: ${errStr(e)}`;
    log.error(`${def.name}: ${result.error}`);
  }

  // Anything the client did not evaluate is a failure.
  for (const s of result.clientSteps) {
    if (s.ok === null) run.set(s.name, false, result.error || "not evaluated");
  }

  if (started) {
    try {
      await pollReport(run);
    } catch (e) {
      if (e instanceof StopError) {
        finish(result, def, run);
        throw e;
      }
      log.error(`${def.name}: report polling failed: ${errStr(e)}`);
    }
  }
  for (const s of result.serverSteps) {
    if (s.ok === null) {
      s.ok = false;
      s.detail = started ? `no server result (state ${result.serverState || "unknown"})` : "script not started";
    }
    log[s.ok ? "info" : "error"](`${s.ok ? "PASS" : "FAIL"} ${def.name}/server/${s.name} ${s.detail}`);
  }

  finish(result, def, run);
  return result.verdict === "PASS";
}

function finish(result: ScriptResult, def: ScriptDef, run: ScriptRun) {
  result.durationMs = Date.now() - result.startedAt;
  const clientOk = result.clientSteps.every((s) => s.ok === true);
  const serverOk = result.serverSteps.every((s) => s.ok === true);
  result.verdict = clientOk && serverOk && result.serverState === "done" ? "PASS" : "FAIL";
  result.status = "done";
  const passed =
    result.clientSteps.filter((s) => s.ok).length + result.serverSteps.filter((s) => s.ok).length;
  const total = result.clientSteps.length + result.serverSteps.length;
  log[result.verdict === "PASS" ? "info" : "error"](
    `${result.verdict} ${def.name}: ${passed}/${total} steps, ${run.fmt(result.durationMs)}, server ${result.serverState || "n/a"}` +
      (result.error ? `, ${result.error}` : "")
  );
}

/**
 * Poll REPORT every second until the server's state is done or aborted,
 * at most 30 s past the nominal duration (counted from t0). Reconnects when
 * needed because REPORT is only readable over the link.
 */
async function pollReport(run: ScriptRun) {
  const { ctx, def, result } = run;
  const deadline = run.t0 + def.nominalMs + 30000;
  let report: Report | null = null;
  for (;;) {
    ctx.check();
    if (!ctx.connected.value) {
      const bound = deadline - Date.now();
      if (bound <= 0 || !(await ctx.reconnectWithin(bound))) break;
      await ctx.restoreSubscriptions().catch(() => {});
    }
    try {
      report = await ctx.readReport();
      result.serverState = report.state;
      if (report.script !== def.id) {
        log.error(`REPORT is for script ${report.script} (${report.name}), expected ${def.id}`);
        report = null;
        break;
      }
      applyReport(result, report);
      if (report.state === "done" || report.state === "aborted") break;
    } catch (e) {
      if (e instanceof StopError) throw e;
      log.warn(`read REPORT failed: ${errStr(e)}`);
    }
    if (Date.now() >= deadline) {
      log.error(`${def.name}: server did not finish within ${(def.nominalMs + 30000) / 1000} s (state ${result.serverState || "unknown"})`);
      break;
    }
    await ctx.sleep(1000);
  }
  if (report) log.info(`REPORT ${JSON.stringify(report)}`);
}

/** Copy the server's step list into the result (names come from the server). */
function applyReport(result: ScriptResult, report: Report) {
  const steps = report.steps.map((s) => ({ name: s.name, ok: s.ok, detail: s.detail }));
  result.serverSteps.splice(0, result.serverSteps.length, ...steps);
}
