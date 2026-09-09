# blec stress client

A Tauri 2 + Vue 3 app that runs the integration scripts of the blec stress protocol
against the chaos GATT server in `examples/stress-server`, exercising
`tauri-plugin-blec` on a phone or a second PC. Each script is a fixed timeline both
sides follow independently; the client merges its own step results with the server's
REPORT into a PASS/FAIL verdict per script. A `hold` soak scenario is kept for long
runs.

The protocol (UUIDs, frame layouts, CONTROL commands, the REPORT JSON and the seven
scripts with their per-step conditions) is described in
[../stress-protocol.md](../stress-protocol.md). The client half of every script lives
in `src/scripts.ts`, the plugin interaction in `src/stress.ts`.

## Setup

Run the server on a Linux host with a Bluetooth adapter:

```sh
cd examples/stress-server
cargo run --release            # advertises as `blec_stress`
```

Run the client on a different Bluetooth adapter (second machine or phone).

### Desktop

```sh
cd examples/stress-client
yarn
yarn tauri dev
```

`@mnlphlp/plugin-blec` is linked from the repository root (`file:../..`). If the import
fails because `dist-js/` is missing, build the JS bindings once in the repository root
with `yarn && yarn build`.

### Android

```sh
cd examples/stress-client
yarn
yarn tauri android init       # once; generates src-tauri/gen/android
yarn tauri android dev
```

Press **Permissions** in the app before the first run to request the Bluetooth /
location runtime permissions.

## Integration run

1. Set the **target name filter** (substring of the advertised name, default
   `blec_stress`, matches `--name` of the server).
2. Scenario **integration** (default). Pick **all** to run the scripts in the run-all
   order 1, 4, 5, 2, 3, 6, 7, or a single script.
3. **Start**. The GATT timeouts from *Advanced* are applied with `setTimeouts()` first
   (defaults connect 20000, discoverServices 15000, read/write/subscribe/disconnect
   10000 ms).

For every script the client

1. restores the **baseline state**: connected to the target and subscribed to ECHO and
   COUNTER (scanning and reconnecting as needed);
2. writes CONTROL `[0x10, script_id]` with response; `t0` is the moment that write
   returns, all step offsets are relative to it;
3. runs its client steps on the timeline; a step that waits for something fails when
   its window elapses;
4. polls REPORT every second (at most 30 s past the nominal duration) until the server
   reports `done` or `aborted`, and copies the server steps into the result;
5. logs `PASS`/`FAIL <script>/<side>/<step> <detail>` for every step, then one
   `PASS`/`FAIL <script>` line. The script passes only if *every* client and server
   step passed and the server state is `done`.

After the last script the log shows `integration: X/N scripts passed`. **Stop** is
cooperative (checked between steps, waits are woken; a plugin call already in flight is
awaited). On stop or at the end the client writes CONTROL `0x11` (abort) so the server
is left idle, then disconnects.

### Scripts

| Id | Name | Nominal | Client steps |
|----|------|---------|--------------|
| 1 | `echo-baseline` | 10 s | `echo_with_response` (20 writes, each verified), `echo_without_response` (20 writes without response sent as a burst, then the 20 echoed frames verified in order) |
| 4 | `notification-storm` | 8 s | `echo_during_storm` (5 echoes from 1.5 s), `storm_received` (>= 500 COUNTER notifications between 1 s and 8 s), `no_seq_gaps` (no COUNTER gap during the script) |
| 5 | `slow-and-fail` | 15 s | `slow_echo` (3 echoes from 0.5 s, rtt >= 1500 ms each), `reads_fail` (2 LARGE reads from 8.5 s fail with a non-timeout error), `read_ok` (3rd LARGE read verified `i & 0xff`), `status_ok` (STATUS `fail_ops_left` = 0) |
| 2 | `link-loss` | 20 s | `disconnect_callback` (exactly one callback 2-5 s, none more until 6 s), `state_false` (connection state false at 6 s), `reconnect` (connected + resubscribed by 17 s), `echo_after_reconnect` (5 echoes) |
| 3 | `disappear` | 25 s | `disconnect_callback` (exactly one 1-4 s), `not_visible` (3 s scan right after the callback does not report the target with an RSSI), `visible_again` (scan started at 8 s, up to 10 s, sees it), `reconnect` (by 22 s), `echo_after_reconnect` (5) |
| 6 | `restart` | 20 s | `disconnect_callback` (exactly one 1-5 s), `reconnect` (scan first, connected + resubscribed by 18 s, `listServices` shows the STRESS service with all characteristics), `echo_after_restart` (5) |
| 7 | `rapid-reconnect` | 60 s | `cycles` (10 x disconnect with exactly one callback within 3 s, connect, subscribe ECHO, 1 echo; all within 60 s), `state_tracking` (connection state false after every disconnect, true after every connect) |

The server steps (`drop`, `client_reconnected`, `storm`, ...) are listed in the spec and
come from REPORT. Echo verification is always: echoed frame within the write timeout,
payload equal, `seq` one higher than the previous echo of the current connection
(0 after a reconnect). Echo payloads are 20 random bytes whose first four bytes carry the
script id and write index (u16 LE each) so a frame can be matched with the server's
`echo` log lines.

### Reading the results table

One row per script: `id name`, verdict (`running`, `PASS`, `FAIL`), passed/total steps
(client + server) and the wall-clock duration including report polling. Tap a row to
expand it: one line per step with side, step name, PASS/FAIL (or `…` while pending) and
the detail text (timings, counts, first failure). A red engine error line appears when
the script could not even start (baseline not reached) or aborted with an exception;
the REPORT state is shown below the steps. The header shows `passed/total`.

Note for desktop clients: BlueZ can keep a cached entry of a peripheral for a while, so
"seen in a scan" always means "reported with a non-null RSSI".

## hold (soak)

Connect, subscribe ECHO and COUNTER and stay connected until stopped. Every COUNTER
notification carries a `u32 LE` sequence number starting at 0 per notify session; every
jump is a `seq gap`. A status line is logged every 10 s. When the link drops
(`disc. unexpected`), the client waits the reconnect backoff, reconnects (rescanning
when the plugin no longer knows the address) and subscribes again. Use the server REPL
(`drop`, `hide`, `storm`, ...) or walk out of range to poke it.

## Counters

| Counter | Meaning |
|---------|---------|
| connect ok / fail | results of `connect()` (a failed connect also counts as op error/timeout) |
| disc. unexpected | disconnect callbacks we did not ask for (server-initiated drops inside a script are expected) |
| notifications | ECHO + COUNTER notifications received |
| echo mismatches | echoed payload differs from what was sent (or frame too short) |
| seq gaps | ECHO or COUNTER sequence number not equal to the expected next value |
| op timeouts | plugin calls that failed with an error containing "timeout"/"Timeout", plus echo notifications that never arrived |
| op errors | every other plugin-call failure |

## Correlating logs

Both sides log one line per event with a unix millisecond timestamp as the first
column. The server prints `<unix_ms> | <event> | <detail>`; the client's **Copy log**
produces `<unix_ms> | <level> | <msg>` including the `PASS`/`FAIL` lines. Paste both
into one file and sort by the first column:

```sh
cat server.log client.log | sort -n > merged.log
```

Make sure both clocks are NTP synced (a phone usually is).
