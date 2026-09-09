# blec stress GATT protocol

Shared by [stress-server](stress-server) (the peripheral running on a Linux host) and
[stress-client](stress-client) (the Tauri app running on the phone or a second PC).

Advertising: local name `blec_stress` (overridable with `--name`), the service UUID list
contains the STRESS service, manufacturer data id `0xB1EC` with value `[0x01]`.

UUIDs (all 128 bit):

| Name    | UUID                                   | Properties                            |
|---------|----------------------------------------|---------------------------------------|
| SERVICE | b1ec5747-0000-4000-8000-000000000000   | primary service                       |
| ECHO    | b1ec5747-0001-4000-8000-000000000000   | write, write-without-response, notify |
| COUNTER | b1ec5747-0002-4000-8000-000000000000   | notify                                |
| LARGE   | b1ec5747-0003-4000-8000-000000000000   | read                                  |
| CONTROL | b1ec5747-0004-4000-8000-000000000000   | write, write-without-response         |
| STATUS  | b1ec5747-0005-4000-8000-000000000000   | read                                  |
| REPORT  | b1ec5747-0006-4000-8000-000000000000   | read                                  |

## ECHO

Every write of payload `P` is answered with one notification `[seq: u32 LE][P]`.
`seq` starts at 0 for every new connection and increments per echoed write.
If the client is not subscribed the write is still accepted (seq still increments).

## COUNTER

While a client is subscribed, the server sends `[seq: u32 LE]` every `interval_ms`
(default 100 ms, `--counter-interval-ms`). `seq` starts at 0 for every notify session.
A "storm" sends `count` extra notifications on COUNTER with the given interval,
continuing the same seq.

## LARGE

Read returns `size` bytes (default 512, `--large-size`), byte `i` == `i & 0xff`.

## CONTROL (client to server, all little endian)

| Byte 0 | Args                             | Action |
|--------|----------------------------------|--------|
| 0x01   | `delay_ms: u16`                  | drop the link to the writing central after `delay_ms` |
| 0x02   | `secs: u8`                       | stop advertising for `secs` seconds ("hide"), then advertise again |
| 0x03   | `count: u16`, `interval_ms: u16` | notification storm on COUNTER |
| 0x04   | `ms: u16`                        | delay every read/write/echo response by `ms` (0 = off) |
| 0x05   | `secs: u8`                       | drop the link, unregister GATT app + advertising, re-register after `secs` |
| 0x06   | `count: u8`                      | fail the next `count` read/write requests with an ATT error |
| 0x10   | `script_id: u8`                  | start the integration script `script_id` (see below); t0 = receipt of this write |
| 0x11   | none                             | abort a running script (report state becomes `aborted`) |

Malformed control writes are logged and ignored (never an error to the client).
Payload lengths must match exactly (3 / 2 / 5 / 3 / 2 / 2 / 2 / 1 bytes).
Pending failures (0x06) apply to ECHO writes and LARGE/STATUS reads, never to CONTROL writes
or REPORT reads, so the client always keeps a working steering channel.
Starting a script while one is running aborts the running one first.

## STATUS

Read returns UTF-8 JSON:

```json
{"uptime_s":0,"connects":0,"drops":0,"slow_ms":0,"hidden":false,"fail_ops_left":0,"echo_seq":0}
```

## REPORT

Read returns the server's view of the last started script as UTF-8 JSON (long read, the
server honours the ATT offset):

```json
{"script":2,"name":"link-loss","state":"running","elapsed_ms":4210,
 "steps":[{"name":"drop","ok":true,"detail":"disconnected AA:BB:CC:DD:EE:FF"},
          {"name":"client_reconnected","ok":null,"detail":"waiting"}]}
```

`state` is `idle` (no script started yet), `running`, `done` or `aborted`. `ok` is `null`
while a step is pending. `steps` lists exactly the server steps defined for the script below,
in order. The client polls REPORT (every second, at most 30 s past the script's nominal
duration) until `state` is `done` or `aborted`, then merges it with its own results.

## Integration scripts

A script is a fixed timeline both sides follow independently. The client's `t0` is the moment
its CONTROL 0x10 write returns; the server's `t0` is the receipt of that write. Times below
are offsets from `t0`. Before starting any script the client is connected and subscribed to
ECHO and COUNTER (the "baseline state"); after a script the client restores that state
before starting the next one. Unless stated otherwise, "echo write" means a 20 byte
write-with-response to ECHO whose echoed frame must arrive within the write timeout, with
the payload equal and `seq` one higher than the previous echo of this connection.

A step passes when its condition holds; a client step that waits for something fails when
its window elapses. The overall verdict of a script is PASS only if every server step and
every client step passed. The run-all order is 1, 4, 5, 2, 3, 6, 7.

### 1 `echo-baseline` (nominal duration 10 s)

| Side   | Step                   | Condition |
|--------|------------------------|-----------|
| client | `echo_with_response`   | 20 echo writes with response, all verified |
| client | `echo_without_response`| 20 echo writes without response (20 byte payloads), 20 echoed frames verified in order |
| server | `echo_writes_received` | 40 ECHO writes received within 10 s |

### 2 `link-loss` (nominal duration 20 s)

| Side   | Step                  | Condition |
|--------|-----------------------|-----------|
| server | `drop`                | at 2 s: link to the central dropped (Disconnect succeeded) |
| client | `disconnect_callback` | exactly one disconnect callback between 2 s and 5 s, no second callback until 6 s |
| client | `state_false`         | connection state reports disconnected by 6 s |
| client | `reconnect`           | reconnected (scan if the address is unknown) and resubscribed by 17 s |
| server | `client_reconnected`  | a central connected within 15 s after the drop |
| client | `echo_after_reconnect`| 5 echo writes verified after reconnecting |
| server | `echo_after_reconnect`| 5 ECHO writes received within 10 s after the reconnect |

### 3 `disappear` (nominal duration 25 s)

| Side   | Step                  | Condition |
|--------|-----------------------|-----------|
| server | `hide`                | at 1 s: advertising stopped for 6 s (until 7 s), then the link is dropped |
| client | `disconnect_callback` | exactly one disconnect callback between 1 s and 4 s |
| client | `not_visible`         | a 3 s scan started right after the callback does not report the target with an RSSI |
| server | `show`                | at 7 s: advertising registered again |
| client | `visible_again`       | a scan started at 8 s (up to 10 s long) reports the target with an RSSI |
| client | `reconnect`           | reconnected and resubscribed by 22 s |
| server | `client_reconnected`  | a central connected within 15 s after re-advertising |
| client | `echo_after_reconnect`| 5 echo writes verified |
| server | `echo_after_reconnect`| 5 ECHO writes received within 10 s after the reconnect |

### 4 `notification-storm` (nominal duration 8 s)

| Side   | Step                | Condition |
|--------|---------------------|-----------|
| server | `storm`             | at 1 s: 500 COUNTER notifications at 5 ms interval sent (notify never errored) |
| client | `echo_during_storm` | 5 echo writes verified, started at 1.5 s |
| client | `storm_received`    | at least 500 COUNTER notifications received between 1 s and 8 s |
| client | `no_seq_gaps`       | no COUNTER sequence gap during the script |

### 5 `slow-and-fail` (nominal duration 15 s)

| Side   | Step                 | Condition |
|--------|----------------------|-----------|
| server | `slow_on`            | at 0 s: response delay set to 1500 ms |
| client | `slow_echo`          | 3 echo writes started at 0.5 s, each round trip at least 1500 ms, all verified |
| server | `slow_echo_received` | 3 ECHO writes received while slow was on (before 8 s) |
| server | `slow_off_fail_armed`| at 8 s: delay set to 0, next 2 read/write requests armed to fail |
| client | `reads_fail`         | 2 LARGE reads started at 8.5 s fail with an error that is not a timeout |
| client | `read_ok`            | the 3rd LARGE read succeeds and every byte `i` equals `i & 0xff` |
| client | `status_ok`          | a STATUS read returns JSON with `fail_ops_left` 0 |
| server | `fails_consumed`     | both armed failures consumed and a successful LARGE read seen by 15 s |

### 6 `restart` (nominal duration 20 s)

| Side   | Step                  | Condition |
|--------|-----------------------|-----------|
| server | `down`                | at 1 s: link dropped, GATT application and advertising unregistered for 4 s |
| client | `disconnect_callback` | exactly one disconnect callback between 1 s and 5 s |
| server | `up`                  | at 5 s: application and advertising registered again |
| client | `reconnect`           | reconnected (scan first) and resubscribed by 18 s; the service list contains the STRESS service with all 6 characteristics |
| server | `client_reconnected`  | a central connected within 15 s after coming up |
| client | `echo_after_restart`  | 5 echo writes verified |
| server | `echo_after_restart`  | 5 ECHO writes received within 10 s after the reconnect |

### 7 `rapid-reconnect` (nominal duration 60 s)

| Side   | Step              | Condition |
|--------|-------------------|-----------|
| client | `cycles`          | 10 cycles of: disconnect (exactly one callback within 3 s), connect, subscribe ECHO, 1 echo write verified; all 10 cycles succeed within 60 s |
| client | `state_tracking`  | connection state reported false after every disconnect and true after every connect |
| server | `connects_seen`   | at least 10 central connections observed within 60 s |
| server | `echo_seen`       | at least 10 ECHO writes received within 60 s |

The server's `done` for a script is reached when its last step resolved (pass or fail);
steps with a window fail when the window elapses.

## Log format

Both programs log one line per event as `<unix_ms> | <event> | <detail>`, e.g.

```
1757400000123 | central_connected | AA:BB:CC:DD:EE:FF
```

Server events: `advertising`, `hidden`, `central_connected`, `central_disconnected`, `drop`,
`echo`, `counter_start`, `counter_stop`, `storm`, `slow`, `fail`, `restart`, `control_invalid`,
`script` (start/step/done lines), plus `info`, `error`, `control`, `read`. Parsers should ignore
unknown event names. The client's log (copy button in the app) uses the levels `info`, `warn`,
`error` as event and prints one `PASS`/`FAIL` line per step and per script.
