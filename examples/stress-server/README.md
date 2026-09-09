# stress-server

A scriptable "chaos" BLE GATT peripheral for stress-testing the `tauri-plugin-blec`
client from a phone or a second PC. It runs on a Linux host with BlueZ and can, on
command or as part of a scripted integration test, drop the link, hide itself (stop
advertising), flood notifications, answer slowly, fail requests, and re-register its
GATT application (simulated reboot).

Built with [`bluer`](https://crates.io/crates/bluer) 0.17 using the callback model,
same structure as `examples/test-server`. The wire protocol and the integration
scripts are shared with `examples/stress-client` and specified in
[`../stress-protocol.md`](../stress-protocol.md); that file is the single source of
truth for UUIDs, byte layouts, JSON shapes and script timelines.

## Requirements

- Linux with BlueZ (`bluetoothd` running, tested against 5.87) and a BLE capable
  adapter (`hci0`). No BlueZ experimental features are required.
- Your user must be allowed to talk to `bluetoothd` over D-Bus (usually a member of the
  `bluetooth` group, or run as root).
- The crate has its own empty `[workspace]` table, so it builds standalone and is not
  part of the plugin's workspace.
- **Disable BlueZ's GATT client role.** By default `bluetoothd` runs service discovery
  on every central that connects, i.e. it opens an ATT client channel to the phone. On
  Android the phone's own GATT server then counts as a holder of the radio link, so a
  client-initiated disconnect only closes the app's channel and the ACL stays up until
  the peripheral disconnects. Symptoms: the phone reports the disconnect callback within
  a few milliseconds, the next connect takes well under a second, the server never logs
  `central_disconnected`, and script 7 `rapid-reconnect` fails with "link never dropped".
  Fix in `/etc/bluetooth/main.conf`:

  ```ini
  [GATT]
  Client = false
  ```

  then `sudo systemctl restart bluetooth`. This only affects this host's ability to act as
  a GATT client (e.g. reading battery levels of BLE accessories), not the server role.
  The server checks this at startup and refuses to run otherwise (`--ignore-bluez-config`
  overrides, `--check-config` only runs the check and exits with 0 or 1).
- **No other GATT server app on the phone.** Any app that registers a `BluetoothGattServer`
  (digital car keys, accessory apps) can count as a holder of every LE link on the phone
  with the same effect as above. `adb shell dumpsys bluetooth_manager | grep "ACL holders"`
  lists the holders per link; force-stop the offending app while testing.
- **Android keeps the ACL for 1 s** after the last GATT client closed before it terminates
  the link. A connect within that second reuses the link, so the server sees neither a
  disconnect nor a new connection. Script 7 waits 1.5 s between disconnect and connect for
  this reason.

## Running

```sh
cd examples/stress-server
cargo run -- --name blec_stress            # commands on stdin, scripts via REPL or CONTROL
RUST_LOG=bluer=debug cargo run             # additionally show bluer's internal logging
```

Flags:

| Flag                      | Default       | Meaning                                |
|---------------------------|---------------|----------------------------------------|
| `--name <NAME>`           | `blec_stress` | advertised local name                  |
| `--adapter <hciN>`        | first adapter | Bluetooth adapter to use               |
| `--counter-interval-ms`   | `100`         | period of COUNTER notifications        |
| `--large-size`            | `512`         | size of the LARGE characteristic value |

Check that it is visible from another machine with `bluetoothctl`:

```
$ bluetoothctl
[bluetooth]# scan on
[NEW] Device XX:XX:XX:XX:XX:XX blec_stress
```

If stdin is closed (started detached) the server keeps running until it is killed and
can be driven entirely over the CONTROL characteristic; `Ctrl-C` or `quit` unregisters
the GATT application and the advertisement before exiting.

## REPL commands

Type these on stdin; each prints `ok` (or `error: ...`).

| Command                        | Action                                                                  |
|--------------------------------|-------------------------------------------------------------------------|
| `drop`                         | disconnect the connected central                                        |
| `hide <secs>`                  | stop advertising for `secs` seconds, then advertise again               |
| `show`                         | start advertising again now                                             |
| `storm <count> <interval_ms>`  | send `count` extra COUNTER notifications with the given spacing         |
| `slow <ms>`                    | delay every read/write/echo response by `ms` (0 = off)                  |
| `fail <count>`                 | fail the next `count` read/write requests with an ATT error             |
| `restart <secs>`               | drop the link, unregister GATT app + advertising, re-register after `secs` |
| `script <id>`                  | start integration script `id` (1-7); aborts a running script first      |
| `abort`                        | abort the running script (report state becomes `aborted`)               |
| `report`                       | print the REPORT JSON                                                   |
| `status`                       | print the STATUS JSON                                                   |
| `help`                         | list commands                                                           |
| `quit`                         | unregister everything and exit                                          |

`hide` and `restart` return immediately; the re-advertise / re-register happens in the
background and is logged when it completes.

## Integration scripts

The seven scripts (`echo-baseline`, `link-loss`, `disappear`, `notification-storm`,
`slow-and-fail`, `restart`, `rapid-reconnect`) are defined in
[`../stress-protocol.md`](../stress-protocol.md#integration-scripts). A script is a
fixed timeline both sides follow independently; the server performs its actions at the
given offsets from `t0` and resolves only the rows marked `server` in the spec tables.

- Start: REPL `script <id>` or CONTROL `0x10 <id>` (t0 = receipt of the write).
  Starting a script while one is running aborts the running one first.
- Abort: REPL `abort` or CONTROL `0x11`.
- Result: the REPORT characteristic (`b1ec5747-0006-4000-8000-000000000000`, read,
  long reads honoured, never slowed and never consumed by the fail counter) returns

  ```json
  {"script":2,"name":"link-loss","state":"running","elapsed_ms":4210,
   "steps":[{"name":"drop","ok":true,"detail":"disconnected AA:BB:CC:DD:EE:FF"},
            {"name":"client_reconnected","ok":null,"detail":"waiting"}]}
  ```

  `state` is `idle` / `running` / `done` / `aborted`; `ok` is `null` while pending.
  Before any script was started it returns
  `{"script":0,"name":"","state":"idle","elapsed_ms":0,"steps":[]}`.

How server steps are resolved:

- Action steps (`drop`, `hide`, `show`, `storm`, `slow_on`, `slow_off_fail_armed`,
  `down`, `up`) pass when the action succeeded at its offset. `hide` (script 3) hides
  for 6 s and then drops the link; `down` (script 6) drops the link before
  unregistering, as does CONTROL `0x05`. `storm` passes only if all 500 notify calls
  succeeded (detail `no COUNTER subscriber` if nobody is subscribed). `up` waits up to
  5 s for both registrations to be back.
- Observation steps wait for a counter delta inside their window and fail when the
  window elapses: `echo_writes_received` (40 ECHO writes within 10 s of t0),
  `client_reconnected` (a new central connection within 15 s of the drop / show / up),
  `echo_after_reconnect` / `echo_after_restart` (5 ECHO writes within 10 s of the
  reconnect; fails immediately if the reconnect did not happen),
  `slow_echo_received` (3 ECHO writes before 8 s), `fails_consumed` (2 failures
  consumed and one successful LARGE read by 15 s), `connects_seen` / `echo_seen`
  (10 connections / 10 ECHO writes within 60 s).

Only accepted ECHO writes count (a write that consumed an armed failure does not); a
LARGE long read counts once (the read at offset 0).

## Log format

One line per event on stdout: `<unix_ms> | <event> | <detail>`, e.g.
`1757400000123 | central_connected | AA:BB:CC:DD:EE:FF`.

Script lines: `script | start id=<id> name=<name>`, `script | step <name> ok=<bool> <detail>`,
`script | done id=<id> pass=<bool>`, `script | aborted`. See the spec for the full
event list; parsers should ignore unknown event names.

## How the connected central is tracked

BlueZ creates a `Device1` object for an incoming central. The server subscribes to
`Adapter::events()` (object added/removed, no active scan) and to each device's
`Device::events()` following the `Connected` property, so `central_connected` /
`central_disconnected` are logged even without GATT traffic. In addition every GATT
callback records `req.device_address`, which covers the case where D-Bus events are
late. `drop` calls `Device::disconnect()` on that address and marks the central as
disconnected as soon as the call returns.
