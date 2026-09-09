<script setup lang="ts">
import { checkPermissions } from "@mnlphlp/plugin-blec";
import { computed, onMounted, reactive, ref } from "vue";
import { entries, log } from "./log";
import { RUN_ALL_ORDER, SCRIPTS, type ScriptResult } from "./scripts";
import { defaultConfig, runner, SCENARIOS, type Counters, type Scenario } from "./stress";

const config = reactive(defaultConfig());
const scenario = ref<Scenario>("integration");
const permission = ref<"unknown" | "granted" | "denied">("unknown");
const copied = ref(false);
const expanded = reactive(new Set<number>());

const counters = runner.counters;
const running = runner.running;
const connected = runner.connected;
const results = runner.results;

const scriptOptions = computed(() => [
  { value: "all", label: `all (${RUN_ALL_ORDER.map((id) => id).join(", ")})` },
  ...RUN_ALL_ORDER.map((id) => {
    const s = SCRIPTS.find((d) => d.id === id)!;
    return { value: String(id), label: `${id} ${s.name} (${s.nominalMs / 1000} s)` };
  }),
]);

const scriptSelect = computed({
  get: () => String(config.script),
  set: (v: string) => {
    config.script = v === "all" ? "all" : Number(v);
  },
});

const counterLabels: { key: keyof Counters; label: string; bad?: boolean }[] = [
  { key: "connectOk", label: "connect ok" },
  { key: "connectFail", label: "connect fail", bad: true },
  { key: "disconnectsUnexpected", label: "disc. unexpected", bad: true },
  { key: "notifications", label: "notifications" },
  { key: "echoMismatches", label: "echo mismatches", bad: true },
  { key: "seqGaps", label: "seq gaps", bad: true },
  { key: "opTimeouts", label: "op timeouts", bad: true },
  { key: "opErrors", label: "op errors", bad: true },
];

const passedCount = computed(() => results.filter((r) => r.verdict === "PASS").length);
const doneCount = computed(() => results.filter((r) => r.status === "done").length);

const logNewestFirst = computed(() => entries.slice().reverse());

onMounted(() => {
  runner.init();
  log.info("blec stress client ready");
});

async function start() {
  // the runner copies the config, so later edits do not affect a running scenario
  await runner.start(scenario.value, JSON.parse(JSON.stringify(config)));
}

function stop() {
  runner.stop();
}

function toggle(id: number) {
  if (expanded.has(id)) expanded.delete(id);
  else expanded.add(id);
}

function stepsPassed(r: ScriptResult): string {
  const all = [...r.clientSteps, ...r.serverSteps];
  return `${all.filter((s) => s.ok === true).length}/${all.length}`;
}

function verdictClass(r: ScriptResult): string {
  if (r.status === "running") return "running";
  if (r.verdict === "PASS") return "pass";
  if (r.verdict === "FAIL") return "fail";
  return "";
}

function verdictText(r: ScriptResult): string {
  if (r.status === "running") return "running";
  return r.verdict ?? "-";
}

function fmtDuration(ms: number): string {
  return ms > 0 ? `${(ms / 1000).toFixed(1)} s` : "-";
}

async function checkPermission() {
  try {
    const ok = await checkPermissions(true);
    permission.value = ok ? "granted" : "denied";
    log.info(`permissions: ${permission.value}`);
  } catch (e) {
    permission.value = "denied";
    log.error(`checkPermissions failed: ${e}`);
  }
}

async function copyLog() {
  try {
    await navigator.clipboard.writeText(log.toText());
    copied.value = true;
    setTimeout(() => (copied.value = false), 1500);
  } catch (e) {
    log.error(`copy failed: ${e}`);
  }
}

function fmtTime(ts: number): string {
  const d = new Date(ts);
  const p = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}
</script>

<template>
  <div class="app">
    <header>
      <h1>blec stress client</h1>
      <span class="badge" :class="{ on: connected }">{{ connected ? "connected" : "disconnected" }}</span>
      <span class="badge" :class="{ on: running }">{{ running ? "running" : "idle" }}</span>
    </header>

    <section class="card">
      <div class="field">
        <label for="filter">Target name filter</label>
        <input id="filter" v-model="config.nameFilter" :disabled="running" />
      </div>
      <div class="grid2">
        <div class="field">
          <label for="scenario">Scenario</label>
          <select id="scenario" v-model="scenario" :disabled="running">
            <option v-for="s in SCENARIOS" :key="s.id" :value="s.id">{{ s.label }}</option>
          </select>
        </div>
        <div class="field" v-if="scenario === 'integration'">
          <label for="script">Script</label>
          <select id="script" v-model="scriptSelect" :disabled="running">
            <option v-for="o in scriptOptions" :key="o.value" :value="o.value">{{ o.label }}</option>
          </select>
        </div>
      </div>

      <details>
        <summary>Advanced (timeouts, scan, backoff)</summary>
        <div class="grid2">
          <div class="field">
            <label for="scanTimeout">Scan timeout ms</label>
            <input id="scanTimeout" type="number" min="1000" v-model.number="config.scanTimeoutMs" :disabled="running" />
          </div>
          <div class="field">
            <label for="backoff">Reconnect backoff ms</label>
            <input id="backoff" type="number" min="0" v-model.number="config.reconnectBackoffMs" :disabled="running" />
          </div>
          <div class="field">
            <label for="t-connect">connect timeout</label>
            <input id="t-connect" type="number" min="0" v-model.number="config.timeouts.connect" :disabled="running" />
          </div>
          <div class="field">
            <label for="t-discover">discoverServices timeout</label>
            <input id="t-discover" type="number" min="0" v-model.number="config.timeouts.discoverServices" :disabled="running" />
          </div>
          <div class="field">
            <label for="t-read">read timeout</label>
            <input id="t-read" type="number" min="0" v-model.number="config.timeouts.read" :disabled="running" />
          </div>
          <div class="field">
            <label for="t-write">write timeout</label>
            <input id="t-write" type="number" min="0" v-model.number="config.timeouts.write" :disabled="running" />
          </div>
          <div class="field">
            <label for="t-subscribe">subscribe timeout</label>
            <input id="t-subscribe" type="number" min="0" v-model.number="config.timeouts.subscribe" :disabled="running" />
          </div>
          <div class="field">
            <label for="t-disconnect">disconnect timeout</label>
            <input id="t-disconnect" type="number" min="0" v-model.number="config.timeouts.disconnect" :disabled="running" />
          </div>
        </div>
      </details>

      <div class="actions">
        <button class="primary" @click="start" :disabled="running">Start</button>
        <button class="danger" @click="stop" :disabled="!running">Stop</button>
        <button @click="checkPermission">
          Permissions<span v-if="permission !== 'unknown'">: {{ permission }}</span>
        </button>
      </div>
    </section>

    <section class="card" v-if="results.length > 0">
      <div class="results-header">
        <h2>Results</h2>
        <span class="summary" :class="{ pass: doneCount === results.length && passedCount === results.length, fail: passedCount < doneCount }">
          {{ passedCount }}/{{ results.length }} passed
        </span>
      </div>
      <div class="results">
        <div v-for="r in results" :key="r.id" class="result" :class="verdictClass(r)">
          <button class="result-row" @click="toggle(r.id)">
            <span class="chevron">{{ expanded.has(r.id) ? "▾" : "▸" }}</span>
            <span class="name">{{ r.id }} {{ r.name }}</span>
            <span class="verdict">{{ verdictText(r) }}</span>
            <span class="steps">{{ stepsPassed(r) }}</span>
            <span class="duration">{{ fmtDuration(r.durationMs) }}</span>
          </button>
          <div v-if="expanded.has(r.id)" class="result-details">
            <div v-if="r.error" class="error-line">{{ r.error }}</div>
            <table>
              <tbody>
                <tr v-for="s in r.clientSteps" :key="'c-' + s.name" :class="{ ok: s.ok === true, bad: s.ok === false }">
                  <td class="side">client</td>
                  <td class="step">{{ s.name }}</td>
                  <td class="mark">{{ s.ok === null ? "…" : s.ok ? "PASS" : "FAIL" }}</td>
                  <td class="detail">{{ s.detail }}</td>
                </tr>
                <tr v-for="s in r.serverSteps" :key="'s-' + s.name" :class="{ ok: s.ok === true, bad: s.ok === false }">
                  <td class="side">server</td>
                  <td class="step">{{ s.name }}</td>
                  <td class="mark">{{ s.ok === null ? "…" : s.ok ? "PASS" : "FAIL" }}</td>
                  <td class="detail">{{ s.detail }}</td>
                </tr>
              </tbody>
            </table>
            <div class="server-state" v-if="r.serverState">server report state: {{ r.serverState }}</div>
          </div>
        </div>
      </div>
    </section>

    <section class="card counters">
      <div v-for="c in counterLabels" :key="c.key" class="counter" :class="{ bad: c.bad && counters[c.key] > 0 }">
        <span class="value">{{ counters[c.key] }}</span>
        <span class="label">{{ c.label }}</span>
      </div>
    </section>

    <section class="card">
      <div class="log-header">
        <h2>Log ({{ entries.length }})</h2>
        <button @click="copyLog">{{ copied ? "Copied" : "Copy log" }}</button>
        <button @click="log.clear()">Clear</button>
      </div>
      <ul class="log">
        <li v-for="(e, i) in logNewestFirst" :key="`${e.ts}-${i}`" :class="e.level">
          <span class="ts">{{ fmtTime(e.ts) }}</span>
          <span class="msg">{{ e.msg }}</span>
        </li>
      </ul>
    </section>
  </div>
</template>

<style scoped>
.app {
  max-width: 760px;
  margin: 0 auto;
  padding: 12px;
  padding-bottom: 32px;
  display: flex;
  flex-direction: column;
  gap: 12px;
  font-family: Inter, Avenir, Helvetica, Arial, sans-serif;
  color: #eaeaea;
}

header {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

h1 {
  font-size: 1.3em;
  margin: 0;
  flex: 1;
}

h2 {
  font-size: 1em;
  margin: 0;
  flex: 1;
}

.badge {
  font-size: 0.8em;
  padding: 4px 10px;
  border-radius: 999px;
  background: #444;
}

.badge.on {
  background: #2f8f4e;
}

.card {
  background: #2a2a2a;
  border-radius: 12px;
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.field {
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
}

.field label {
  font-size: 0.85em;
  opacity: 0.8;
}

.grid2 {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 10px;
}

input,
select,
button {
  font: inherit;
  font-size: 1em;
  color: #fff;
  background: #1a1a1a;
  border: 1px solid #444;
  border-radius: 8px;
  padding: 10px 12px;
  min-height: 44px;
  box-sizing: border-box;
  width: 100%;
}

input:disabled,
select:disabled {
  opacity: 0.5;
}

details summary {
  cursor: pointer;
  padding: 8px 0;
  opacity: 0.85;
}

details .grid2 {
  margin-top: 8px;
}

.actions {
  display: grid;
  grid-template-columns: 1fr 1fr 1fr;
  gap: 10px;
}

button {
  cursor: pointer;
  font-weight: 600;
}

button:disabled {
  opacity: 0.4;
  cursor: default;
}

button.primary {
  background: #2f8f4e;
  border-color: #2f8f4e;
}

button.danger {
  background: #b23a3a;
  border-color: #b23a3a;
}

/* results */
.results-header {
  display: flex;
  align-items: center;
  gap: 8px;
}

.summary {
  font-weight: 700;
}

.summary.pass {
  color: #6fd98f;
}

.summary.fail {
  color: #ff7b7b;
}

.results {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.result {
  background: #1f1f1f;
  border-radius: 8px;
  border-left: 4px solid #555;
  overflow: hidden;
}

.result.pass {
  border-left-color: #2f8f4e;
}

.result.fail {
  border-left-color: #b23a3a;
}

.result.running {
  border-left-color: #d9a441;
}

.result-row {
  display: grid;
  grid-template-columns: 1.2em 1fr auto auto auto;
  gap: 8px;
  align-items: center;
  text-align: left;
  background: transparent;
  border: none;
  border-radius: 0;
  font-weight: 500;
}

.result-row .chevron {
  opacity: 0.6;
}

.result-row .name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.result-row .verdict {
  font-weight: 700;
  font-size: 0.85em;
}

.result.pass .verdict {
  color: #6fd98f;
}

.result.fail .verdict {
  color: #ff7b7b;
}

.result.running .verdict {
  color: #d9a441;
}

.result-row .steps,
.result-row .duration {
  font-size: 0.85em;
  opacity: 0.8;
  font-variant-numeric: tabular-nums;
}

.result-details {
  padding: 4px 10px 10px 10px;
  overflow-x: auto;
}

.result-details table {
  width: 100%;
  border-collapse: collapse;
  font-size: 0.82em;
}

.result-details td {
  padding: 4px 6px;
  border-top: 1px solid #333;
  vertical-align: top;
}

.result-details td.side {
  opacity: 0.6;
  white-space: nowrap;
}

.result-details td.step {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  white-space: nowrap;
}

.result-details td.mark {
  font-weight: 700;
  white-space: nowrap;
}

.result-details tr.ok td.mark {
  color: #6fd98f;
}

.result-details tr.bad td.mark {
  color: #ff7b7b;
}

.result-details td.detail {
  word-break: break-word;
  opacity: 0.9;
}

.error-line {
  color: #ff7b7b;
  font-size: 0.85em;
  padding: 4px 0;
}

.server-state {
  font-size: 0.78em;
  opacity: 0.6;
  padding-top: 6px;
}

/* counters */
.counters {
  display: grid;
  grid-template-columns: repeat(4, minmax(0, 1fr));
  gap: 8px;
}

.counter {
  display: flex;
  flex-direction: column;
  align-items: center;
  padding: 6px 4px;
  background: #1f1f1f;
  border-radius: 8px;
}

.counter.bad {
  background: #4a2323;
}

.counter .value {
  font-size: 1.2em;
  font-weight: 700;
  font-variant-numeric: tabular-nums;
}

.counter .label {
  font-size: 0.7em;
  opacity: 0.75;
  text-align: center;
}

/* log */
.log-header {
  display: flex;
  align-items: center;
  gap: 8px;
}

.log-header button {
  width: auto;
  min-height: 36px;
  padding: 6px 12px;
}

.log {
  list-style: none;
  margin: 0;
  padding: 0;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 0.78em;
  max-height: 50vh;
  overflow-y: auto;
  overflow-x: auto;
}

.log li {
  display: flex;
  gap: 8px;
  padding: 3px 0;
  border-bottom: 1px solid #333;
  white-space: pre-wrap;
  word-break: break-word;
}

.log .ts {
  opacity: 0.6;
  flex-shrink: 0;
}

.log li.warn {
  color: #f0c060;
}

.log li.error {
  color: #ff7b7b;
}

@media (max-width: 420px) {
  .counters {
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }

  .result-row {
    grid-template-columns: 1.2em 1fr auto;
  }

  .result-row .duration {
    display: none;
  }
}
</style>

<style>
:root {
  color-scheme: dark;
  background-color: #1c1c1c;
  -webkit-text-size-adjust: 100%;
}

body {
  margin: 0;
}
</style>
