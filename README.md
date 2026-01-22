# Tauri Plugin blec

A BLE-Client plugin based on [btlelug](https://github.com/deviceplug/btleplug).

The main difference to using btleplug directly is that this uses the tauri plugin system for android.
All other platforms use the btleplug implementation.

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
tauri-plugin-blec = "0.4"
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

## Usage in Frontend

See [examples/plugin-blec-example](examples/plugin-blec-example) for a full working example that scans for devices, connects and sends/receives data.
In order to use it run [examples/test-server](examples/test-server) on another device and connect to that server.

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
    .send_data(CHARACTERISTIC_UUID, &DATA, WriteType::WithResponse)
    .await
    .unwrap();
```
