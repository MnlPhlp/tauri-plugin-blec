# Tauri Plugin blec

A BLE-Client plugin based on [btlelug](https://github.com/deviceplug/btleplug).

The main difference to using btleplug directly is that this ships its own android backend.
All other platforms use the btleplug implementation.

This crate is a thin Tauri layer over [`blec`](../blec), which holds the BLE handler and the
platform backends. The Rust API here is `blec`'s, re-exported, so `tauri_plugin_blec::get_handler()`
and friends keep working. Use `blec` directly in a non-Tauri rust app.

## Docs

- [Rust docs](https://docs.rs/crate/tauri-plugin-blec/latest)
- [JavaScript docs](https://mnlphlp.github.io/tauri-plugin-blec/)

## Installation

### Install the rust part of the plugin

```bash
cargo add tauri-plugin-blec
```

Or manually add it to the `src-tauri/Cargo.toml`

```toml
[dependencies]
tauri-plugin-blec = "0.17.0" # x-release-please-version
```

### Install the js bindings

use your preferred JavaScript package manager to add `@mnlphlp/plugin-blec`:

```bash
yarn add @mnlphlp/plugin-blec
```

```bash
npm add @mnlphlp/plugin-blec
```

### Register the plugin in Tauri

`src-tauri/src/lib.rs`

```rs
tauri::Builder::default()
    .plugin(tauri_plugin_blec::init())
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
```

```rs
let mut app = tauri::Builder::default();
app = match tauri_plugin_blec::try_init() {
    Ok(plugin) => app.plugin(plugin),
    Err(e) => {
        eprintln!("Failed to initialize blec plugin: {:?}", e);
        app
    }
};
app.run(tauri::generate_context!())
    .expect("error while running tauri application");
```

### Allow calls from Frontend

Add `blec:default` to the permissions in your capabilities file.

[Explanation about capabilities](https://v2.tauri.app/security/capabilities/)

### Android Setup

Nothing to do. The plugin's android module (`android/`, shared with the `blec` crate) carries the
bluetooth permissions and the Kotlin side of the BLE client; tauri builds it into your app like any
other plugin module, so the app never loads code at runtime. See
[Android internals](../blec/README.md#android-internals) for how it is wired and the main `Looper`
requirement.

Call `checkPermissions(true)` from the frontend (or `blec::check_permissions(true)` from rust)
before scanning; it asks the user and, if they denied before, sends them to the app settings.

### IOS Setup

Add an entry to the info.plist of your app:

```xml
<key>NSBluetoothAlwaysUsageDescription</key>
<string>The App uses Bluetooth to communicate with BLE devices</string>
```

Add the CoreBluetooth Framework in your xcode procjet:

- open with `tauri ios dev --open`
- click on your project to open settings
- Add Framework under General -> Frameworks,Libraries and Embedded Content

## Upgrading to 0.16

The crate was split: the BLE client now lives in [`blec`](../blec) and this crate is a thin tauri
layer over it, re-exporting the whole rust API. Existing code keeps working, with two exceptions:

- `check_permissions` is `async` now (it waits for the user). The JavaScript API is unchanged.
- `Error::PluginInvoke` is gone; android failures are `Error::Android(String)`.

In a non-tauri rust app, depend on `blec` directly instead.

## Usage in Frontend

See [examples/plugin-blec-example](../../examples/plugin-blec-example) for a full working example that scans for devices, connects and sends/receives data.
In order to use it run [examples/test-server](../../examples/test-server) on another device and connect to that server.

Short example:

```ts
import { connect, sendString } from '@mnlphlp/plugin-blec'
// get address by scanning for devices and selecting the desired one
let address = ...
// connect and run a callback on disconnect
await connect(address, () => console.log('disconnected'))
// send some text to a characteristic
const CHARACTERISTIC_UUID = '51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B'
await sendString(CHARACTERISTIC_UUID, 'Test', 'withResponse')
```

## Testing

### Unit tests with the mock backend

The crate contains a mock of the btleplug backend (`tauri_plugin_blec::mock`) that simulates
an adapter and devices in process: devices can disappear, drop their link, answer slowly, fail
or hang operations, and send notifications. The handler tests in `crates/blec/src/handler/tests` drive the
plugin through those situations, including a seeded randomized stress test.

```bash
cargo test --workspace
# with the handler's logs
RUST_LOG=trace cargo test -- --nocapture
# longer randomized run, reproducible via the seed
BLEC_STRESS_SEED=7 BLEC_STRESS_ITERS=5000 cargo test stress -- --nocapture
# tests documenting known bugs are ignored; run them to see the current behaviour
cargo test -- --ignored
```

The mock can also replace the real backend in an app (desktop targets only) with the `mock`
cargo feature. The plugin then talks to `MockWorld::global()`, which the app can script:

```rs
use btleplug::api::CharPropFlags;
use tauri_plugin_blec::mock::{DeviceSpec, MockWorld, ServiceSpec};
let device = MockWorld::global().add_device(
    DeviceSpec::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01]).name("fake").service(
        ServiceSpec::new(SERVICE_UUID).characteristic(CHARACTERISTIC_UUID, CharPropFlags::NOTIFY),
    ),
);
device.drop_link(); // simulate a lost connection
```

### Stress testing the full stack

For the real stack there are two example programs:

- [examples/stress-server](../../examples/stress-server): a GATT peripheral (Linux/BlueZ, runs on the
  host PC) that can drop the link, stop advertising, flood notifications, answer slowly, fail
  requests or restart, on request from the client or from its REPL.
- [examples/stress-client](../../examples/stress-client): a Tauri app for the phone or a second PC.
  Its `integration` scenario runs predefined scripts: one write starts a script, both sides
  follow the same timeline (drop, disappear, storm, slow responses, restart, rapid reconnects),
  verify their side and the client merges the server's report into a PASS/FAIL table.
  A `hold` scenario keeps a connection open for soak testing.

The scripts and the GATT protocol are defined in
[examples/stress-protocol.md](../../examples/stress-protocol.md).

## Usage in Backend

The plugin can also be used from the rust backend.

The handler returned by `get_handler()` is the same that is used by the frontend commands.
This means if you connect from the frontend you can send data from rust without having to call connect on the backend.

```rs
use uuid::{uuid, Uuid};
use tauri_plugin_blec::models::WriteType;

const CHARACTERISTIC_UUID: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");
const DATA: [u8; 500] = [0; 500];
let handler = tauri_plugin_blec::get_handler().unwrap();
handler
    // `None` searches every service for the characteristic; pass `Some(uuid)`
    // when two services share one.
    .send_data(CHARACTERISTIC_UUID, None, &DATA, WriteType::WithResponse)
    .await
    .unwrap();
```
