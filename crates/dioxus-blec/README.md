# dioxus-blec

[`blec`](../blec), the cross platform BLE (Bluetooth Low Energy) client, for
[Dioxus](https://dioxuslabs.com) 0.7.

- `use_ble()` initializes `blec` once for the app and mirrors its state into signals: ready,
  scanning, found devices, connected device. It has async methods for scanning, connecting,
  reading and writing.
- `use_ble_notifications(characteristic, service)` follows a characteristic for as long as the
  component lives, resubscribing after every reconnect.
- On android the crate carries the bluetooth permissions and the Kotlin side of the BLE client as
  a gradle module that `dx` builds into the app, so there is no manifest to write and no gradle
  module to add.

The full `blec` API stays available through `ble.handler()` and the re-exported `dioxus_blec::blec`.

## Usage

```toml
[dependencies]
dioxus-blec = "0.15"
```

```rust
use dioxus::prelude::*;
use dioxus_blec::models::{ScanFilter, WriteType};
use dioxus_blec::{use_ble, use_ble_notifications, InitState};
use uuid::{uuid, Uuid};

const CHARACTERISTIC: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");

#[component]
fn App() -> Element {
    let ble = use_ble();
    let notified = use_ble_notifications(CHARACTERISTIC, None);

    if let InitState::Failed(e) = ble.init_state() {
        return rsx! { p { "bluetooth unavailable: {e}" } };
    }

    rsx! {
        button {
            disabled: !ble.is_ready() || ble.scanning(),
            onclick: move |_| async move {
                if let Err(e) = ble.scan(5000, ScanFilter::None).await {
                    tracing::error!("scan failed: {e}");
                }
            },
            "Scan"
        }
        if let Some(device) = ble.connected() {
            p { "connected to {device.name}, last notification: {notified():?}" }
            button {
                onclick: move |_| async move {
                    let _ = ble.write(CHARACTERISTIC, None, b"hello", WriteType::WithResponse).await;
                },
                "Send"
            }
            button { onclick: move |_| async move { let _ = ble.disconnect().await; }, "Disconnect" }
        } else {
            ul {
                for device in ble.devices() {
                    li {
                        key: "{device.address}",
                        button {
                            onclick: move |_| {
                                let address = device.address.clone();
                                async move { let _ = ble.connect(&address).await; }
                            },
                            "{device.name} ({device.address})"
                        }
                    }
                }
            }
        }
    }
}
```

`Ble` is `Copy`, like a signal. Reading one of its getters in a component subscribes the component
to that piece of state. The async methods spawn their forwarding tasks on the Dioxus runtime, so
call them from components and event handlers, not from a plain tokio task.

[`../../examples/dioxus-example`](../../examples/dioxus-example) is a complete app.

## Android

The only android setting the app needs is the minimum sdk `blec` requires:

```toml
# Dioxus.toml
[android]
min_sdk = 26
```

Everything else is in this crate. `#[manganis::ffi("android")]` in `src/lib.rs` embeds the path of
the crate's `android/` directory in the binary. `dx` picks it up when building for android, copies
the directory into the generated gradle project as a library module and builds it with the app. The
module's `AndroidManifest.xml` is merged into the app's; it declares the permissions with the
attributes `[android.permissions]` in `Dioxus.toml` cannot express (`neverForLocation` on
`BLUETOOTH_SCAN`, `maxSdkVersion` on the legacy `BLUETOOTH`) and the `bluetooth_le` feature. The
module's Kotlin is the android backend of `blec`, which finds it in the app's class loader at
startup. The directory is a symlink to `crates/blec/android/lib`, the one copy of that module; see
[Android internals](../blec/README.md#android-internals).

Runtime permissions are requested by the first scan, or explicitly with
`ble.check_permissions(ask_if_denied)`.

Build for an attached phone with `dx serve --platform android --device`; without `--device`, `dx`
builds for the emulator (`x86_64`) even when none is running.

## Without a bluetooth adapter

The `mock` feature (desktop only) replaces the platform backend with `blec::mock`, an in-process
simulation with no devices until the app adds some. See the mock section of the
[`blec` README](../blec/README.md#mock-backend).

## License

MIT or Apache-2.0, at your option.
